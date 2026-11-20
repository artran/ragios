use crate::monitor::SchedulerFault;
use axum::{
    Router,
    extract::{Path, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::get,
};
use chrono::{DateTime, SecondsFormat, Utc};
use ragios::{
    config::{Check, Config, Host, HostGroup, Probe, Service},
    runner::{CheckResult, Reachability, Status},
    schedule,
    store::{
        LatestHosts, LatestResults, ProblemState, Store, TRANSITION_RETENTION, Transition,
        TransitionKind,
    },
};
use serde::Deserialize;
use std::{fmt::Write, sync::Arc};
use tokio::sync::watch;

#[derive(Clone)]
struct Dashboard {
    config: watch::Receiver<Arc<Config>>,
    faults: watch::Receiver<Option<SchedulerFault>>,
    store: Store,
}

type WebError = (StatusCode, &'static str);
type WebResult = Result<Html<String>, WebError>;

pub fn router(
    config: watch::Receiver<Arc<Config>>,
    faults: watch::Receiver<Option<SchedulerFault>>,
    store: Store,
) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/status", get(status))
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
            "/assets/htmx.min.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../assets/htmx.min.js"),
                )
            }),
        )
        .with_state(Dashboard {
            config,
            faults,
            store,
        })
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
    let snapshot = snapshot(&dashboard).await?;
    let fragment = overview(&snapshot, &filters);
    let mut form = String::from(
        r##"<form id="filters" action="/" method="get" hx-get="/status" hx-target="#status" hx-trigger="change, submit" hx-sync="this:replace"><label>Host group <select name="group"><option value="">All groups</option>"##,
    );
    for group in &snapshot.config.groups {
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
            r##"<h1>Ragios</h1><p>Host reachability and service health · email disabled</p>
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
    let fragment = overview(&snapshot(&dashboard).await?, &filters);
    let mut response = Html(fragment).into_response();
    // A filter change keeps the address bar shareable; polling leaves it alone.
    if headers.get("hx-trigger").is_some_and(|id| id == "filters")
        && let Ok(url) = HeaderValue::from_str(&format!("/?{}", filters.query()))
    {
        response.headers_mut().insert("hx-replace-url", url);
    }
    Ok(response)
}

fn load_error(error: anyhow::Error) -> WebError {
    eprintln!("load dashboard results: {error:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Cannot load monitoring results; consult the server log.",
    )
}

struct Snapshot {
    config: Arc<Config>,
    services: LatestResults,
    hosts: LatestHosts,
    fault: Option<SchedulerFault>,
    now: DateTime<Utc>,
}

impl Snapshot {
    fn host(&self, id: &str) -> Option<&Host> {
        self.config.hosts.iter().find(|host| host.id == id)
    }

    fn freshness(&self, check: &Check, state: Option<(&CheckResult, ProblemState)>) -> Freshness {
        let Some((result, problem)) = state else {
            return Freshness::NotYetChecked;
        };
        // An unreadable timestamp cannot establish current monitoring.
        let Some(finished) = result.finished() else {
            return Freshness::Overdue(None);
        };
        let due = schedule::due_at(check, self.config.retry_interval_seconds, finished, problem);
        if schedule::is_overdue(check, due, self.now) {
            Freshness::Overdue(Some(due))
        } else {
            Freshness::Current(due)
        }
    }

    fn overdue_count(&self) -> usize {
        let mut count = 0;
        for host in &self.config.hosts {
            if let Some(probe) = &host.reachability {
                let state = self.hosts.get(&host.id);
                let state = state.map(|state| (&state.result, state.problem));
                count += usize::from(self.freshness(&probe.check, state).is_overdue());
            }
            for service in &host.services {
                let state = self.services.get(&(host.id.clone(), service.id.clone()));
                let state = state.map(|state| (&state.result, state.problem));
                count += usize::from(self.freshness(&service.check, state).is_overdue());
            }
        }
        count
    }
}

/// Whether a latest result still reflects current monitoring. Freshness is
/// assessed from when the check is due, never from the plugin's status.
#[derive(Clone, Copy)]
enum Freshness {
    NotYetChecked,
    Current(DateTime<Utc>),
    Overdue(Option<DateTime<Utc>>),
}

impl Freshness {
    fn is_overdue(self) -> bool {
        matches!(self, Self::Overdue(_))
    }

    fn cell(self) -> String {
        let time = |due: DateTime<Utc>| due.to_rfc3339_opts(SecondsFormat::Millis, true);
        match self {
            Self::NotYetChecked => "<td>Not yet checked</td>".into(),
            Self::Current(due) => format!("<td>Due {}</td>", time(due)),
            Self::Overdue(Some(due)) => format!(
                "<td class=\"stale\">Overdue since {}; latest result is stale</td>",
                time(due)
            ),
            Self::Overdue(None) => {
                "<td class=\"stale\">Overdue; due time unknown, latest result is stale</td>".into()
            }
        }
    }
}

/// Banners for conditions that stop monitoring from making progress.
fn alerts(snapshot: &Snapshot) -> String {
    let mut html = String::new();
    if let Some(fault) = &snapshot.fault {
        let _ = write!(
            html,
            r#"<p class="alert">Scheduler fault since {}: {}. Monitoring is not recording progress.</p>"#,
            fault.since.to_rfc3339_opts(SecondsFormat::Millis, true),
            escape(&fault.message)
        );
    }
    match snapshot.overdue_count() {
        0 => {}
        1 => {
            html.push_str(r#"<p class="alert">1 check is overdue; its latest result is stale.</p>"#)
        }
        count => {
            let _ = write!(
                html,
                r#"<p class="alert">{count} checks are overdue; their latest results are stale.</p>"#
            );
        }
    }
    html
}

async fn snapshot(dashboard: &Dashboard) -> Result<Snapshot, WebError> {
    // If a reload spans the database read, retry with the newly published
    // definitions. Otherwise old names could be paired with new-command results
    // that were never active together. Rendering has no further awaits.
    loop {
        let config = dashboard.config.borrow().clone();
        let services = dashboard.store.latest().await.map_err(load_error)?;
        let hosts = dashboard.store.latest_hosts().await.map_err(load_error)?;
        if Arc::ptr_eq(&config, &dashboard.config.borrow()) {
            return Ok(Snapshot {
                config,
                services,
                hosts,
                fault: dashboard.faults.borrow().clone(),
                now: Utc::now(),
            });
        }
    }
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
    let Snapshot {
        config,
        services,
        hosts,
        ..
    } = snapshot;
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
                    let state = state.map(|state| (&state.result, state.problem));
                    render_result(
                        &mut rows,
                        state,
                        snapshot.freshness(&probe.check, state),
                        CheckDisplay::Reachability(
                            hosts.get(&host.id).map_or(probe.probe, |state| state.probe),
                        ),
                        config.confirmation_attempts,
                    );
                }
            }
            for service in &host.services {
                let state = services.get(&(host.id.clone(), service.id.clone()));
                if problem.matches(state.map(|state| state.problem)) {
                    row_start(
                        &mut rows,
                        group,
                        host,
                        &service.name,
                        &service.id,
                        &service_url(host, service),
                    );
                    let state = state.map(|state| (&state.result, state.problem));
                    render_result(
                        &mut rows,
                        state,
                        snapshot.freshness(&service.check, state),
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

fn confirmed_cell(problem: ProblemState, display: CheckDisplay) -> String {
    match problem.confirmed_severity {
        Some(severity) => format!(
            "<td class=\"{}\">Confirmed problem: {}</td>",
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
    freshness: Freshness,
    display: CheckDisplay,
    confirmation_attempts: u32,
) {
    if let Some((result, problem)) = state {
        html.push_str(&latest_cell(result, display, freshness));
        html.push_str(&freshness.cell());
        html.push_str(&confirmed_cell(problem, display));
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
        html.push_str("<td>Not yet checked</td><td colspan=\"7\">No result available</td>");
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
    let snapshot = snapshot(&dashboard).await?;
    let Some(host) = snapshot.host(&host_id) else {
        return Ok(not_configured("host"));
    };
    let heading = host_heading(&snapshot.config, host);
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
    let snapshot = snapshot(&dashboard).await?;
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
    freshness: Freshness,
}

impl<'a> CheckDetail<'a> {
    fn service(snapshot: &'a Snapshot, host_id: &str, service_id: &str) -> Option<Self> {
        let host = snapshot.host(host_id)?;
        let service = host
            .services
            .iter()
            .find(|service| service.id == service_id)?;
        let state = snapshot
            .services
            .get(&(host.id.clone(), service.id.clone()))
            .map(|state| (&state.result, state.problem));
        Some(Self {
            host,
            name: &service.name,
            subtitle: service.id.clone(),
            url: service_url(host, service),
            display: CheckDisplay::Service,
            state,
            freshness: snapshot.freshness(&service.check, state),
        })
    }

    fn reachability(snapshot: &'a Snapshot, host_id: &str) -> Option<Self> {
        let host = snapshot.host(host_id)?;
        let probe = host.reachability.as_ref()?;
        let saved = snapshot.hosts.get(&host.id);
        let state = saved.map(|state| (&state.result, state.problem));
        Some(Self {
            host,
            name: "Host reachability",
            subtitle: format!("Configured probe: {}", probe.probe.label()),
            url: reachability_url(host),
            display: CheckDisplay::Reachability(saved.map_or(probe.probe, |state| state.probe)),
            state,
            freshness: snapshot.freshness(&probe.check, state),
        })
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

    fn fragment(&self, snapshot: &Snapshot, transitions: &[Transition]) -> String {
        let confirmation_attempts = snapshot.config.confirmation_attempts;
        let mut html = alerts(snapshot);
        html.push_str("<h2>Current state</h2><table class=\"facts\"><tbody>");
        match self.state {
            Some((result, problem)) => {
                let _ = write!(
                    html,
                    "<tr><th>Latest result</th>{}</tr><tr><th>Freshness</th>{}</tr><tr><th>Confirmed problem</th>{}</tr><tr><th>Pending problem / severity change</th>{}</tr><tr><th>Output</th><td><pre>{}</pre></td></tr><tr><th>Started (UTC)</th><td>{}</td></tr><tr><th>Finished (UTC)</th><td>{}</td></tr><tr><th>Duration</th><td>{} ms</td></tr><tr><th>Result age</th><td>{}</td></tr>",
                    latest_cell(result, self.display, self.freshness),
                    self.freshness.cell(),
                    confirmed_cell(problem, self.display),
                    pending_cell(problem, self.display, confirmation_attempts),
                    escape(&result.output),
                    escape(&result.started_at),
                    escape(&result.finished_at),
                    result.duration_ms,
                    age(result, snapshot.now)
                );
            }
            None => html.push_str("<tr><th>Latest result</th><td>Not yet checked</td></tr>"),
        }
        html.push_str("</tbody></table><h2>State changes</h2>");
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
                confirmed_cell(transition.state, self.display)
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

enum CheckPath {
    Service(String, String),
    Reachability(String),
}

async fn check_detail(
    dashboard: &Dashboard,
    path: CheckPath,
    fragment_only: bool,
) -> Result<Response, WebError> {
    let snapshot = snapshot(dashboard).await?;
    let detail = match &path {
        CheckPath::Service(host, service) => CheckDetail::service(&snapshot, host, service),
        CheckPath::Reachability(host) => CheckDetail::reachability(&snapshot, host),
    };
    let Some(detail) = detail else {
        return Ok(not_configured("check"));
    };
    let transitions = match &path {
        CheckPath::Service(host, service) => {
            dashboard
                .store
                .transitions(host.clone(), service.clone())
                .await
        }
        CheckPath::Reachability(host) => dashboard.store.host_transitions(host.clone()).await,
    }
    .map_err(load_error)?;
    let fragment = detail.fragment(&snapshot, &transitions);
    Ok(if fragment_only {
        Html(fragment).into_response()
    } else {
        detail_page(detail.name, &detail.heading(), &detail.url, &fragment)
    })
}

async fn service_page(
    State(dashboard): State<Dashboard>,
    Path((host, service)): Path<(String, String)>,
) -> Result<Response, WebError> {
    check_detail(&dashboard, CheckPath::Service(host, service), false).await
}

async fn service_status(
    State(dashboard): State<Dashboard>,
    Path((host, service)): Path<(String, String)>,
) -> Result<Response, WebError> {
    check_detail(&dashboard, CheckPath::Service(host, service), true).await
}

async fn reachability_page(
    State(dashboard): State<Dashboard>,
    Path(host): Path<String>,
) -> Result<Response, WebError> {
    check_detail(&dashboard, CheckPath::Reachability(host), false).await
}

async fn reachability_status(
    State(dashboard): State<Dashboard>,
    Path(host): Path<String>,
) -> Result<Response, WebError> {
    check_detail(&dashboard, CheckPath::Reachability(host), true).await
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
