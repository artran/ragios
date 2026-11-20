use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::Html,
    routing::get,
};
use ragios::{
    config::{Config, Host, HostGroup, Probe},
    runner::{CheckResult, Reachability, Status},
    store::{ProblemState, Store},
};
use std::{fmt::Write, sync::Arc};

#[derive(Clone)]
struct Dashboard {
    config: Arc<Config>,
    store: Store,
}

type WebResult = Result<Html<String>, (StatusCode, &'static str)>;

pub fn router(config: Arc<Config>, store: Store) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/status", get(status))
        .route(
            "/assets/htmx.min.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../assets/htmx.min.js"),
                )
            }),
        )
        .with_state(Dashboard { config, store })
}

async fn page(State(dashboard): State<Dashboard>) -> WebResult {
    let fragment = render(&dashboard).await?;
    Ok(Html(format!(
        r#"<!doctype html>
<html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Ragios — host reachability and service health</title><script src="/assets/htmx.min.js" defer></script>
<style>
body{{font:1rem system-ui,sans-serif;margin:2rem;background:#f5f7fa;color:#152336}}
main{{max-width:90rem;margin:auto}}table{{width:100%;border-collapse:collapse;background:white}}
th,td{{padding:.7rem;text-align:left;border-bottom:1px solid #d8dee8;vertical-align:top}}
pre{{white-space:pre-wrap;overflow-wrap:anywhere;margin:0;max-width:40rem}}
.OK{{color:#146138}}.WARNING{{color:#805000}}.CRITICAL{{color:#b02020}}.UNKNOWN{{color:#654398}}
small{{color:#45556b}}.scroll{{overflow-x:auto}}
</style></head><body><main><h1>Ragios</h1><p>Host reachability and service health · email disabled</p>
<p>Latest results, confirmed problems, and pending changes are shown separately.
Reachability is assessed from this monitoring server, independently of service health.
A failed probe does not prove the machine is powered off. Service checks continue regardless of host reachability.</p>
<div id="status" hx-get="/status" hx-trigger="every 2s" hx-swap="innerHTML">{fragment}</div>
<noscript>Reload this page to refresh results.</noscript></main></body></html>"#
    )))
}

async fn status(State(dashboard): State<Dashboard>) -> WebResult {
    Ok(Html(render(&dashboard).await?))
}

fn load_error(error: anyhow::Error) -> (StatusCode, &'static str) {
    eprintln!("load dashboard results: {error:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Cannot load monitoring results; consult the server log.",
    )
}

async fn render(dashboard: &Dashboard) -> Result<String, (StatusCode, &'static str)> {
    let latest = dashboard.store.latest().await.map_err(load_error)?;
    let hosts = dashboard.store.latest_hosts().await.map_err(load_error)?;
    let mut html = String::from(
        "<div class=\"scroll\"><table><thead><tr><th>Group</th><th>Host</th><th>Check</th><th>Latest result</th><th>Confirmed problem</th><th>Pending problem / severity change</th><th>Output</th><th>Started (UTC)</th><th>Finished (UTC)</th><th>Duration</th></tr></thead><tbody>",
    );
    for group in &dashboard.config.groups {
        for host in dashboard
            .config
            .hosts
            .iter()
            .filter(|host| host.group == group.id)
        {
            if let Some(probe) = &host.reachability {
                row_start(
                    &mut html,
                    group,
                    host,
                    "Host reachability",
                    &format!("Configured probe: {}", probe.probe.label()),
                );
                render_result(
                    &mut html,
                    hosts
                        .get(&host.id)
                        .map(|state| (&state.result, state.problem)),
                    CheckDisplay::Reachability(
                        hosts
                            .get(&host.id)
                            .map(|state| state.probe)
                            .unwrap_or(probe.probe),
                    ),
                    dashboard.config.confirmation_attempts,
                );
            }
            for service in &host.services {
                row_start(&mut html, group, host, &service.name, &service.id);
                render_result(
                    &mut html,
                    latest
                        .get(&(host.id.clone(), service.id.clone()))
                        .map(|state| (&state.result, state.problem)),
                    CheckDisplay::Service,
                    dashboard.config.confirmation_attempts,
                );
            }
        }
    }
    html.push_str("</tbody></table></div>");
    Ok(html)
}

fn row_start(html: &mut String, group: &HostGroup, host: &Host, name: &str, id: &str) {
    let _ = write!(
        html,
        "<tr><td>{}<br><small>{}</small></td><td>{}<br><small>{}</small>{}</td><td>{}<br><small>{}</small></td>",
        escape(&group.name),
        escape(&group.id),
        escape(&host.name),
        escape(&host.id),
        if host.reachability.is_none() {
            "<br><small>Reachability not configured</small>"
        } else {
            ""
        },
        escape(name),
        escape(id)
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

fn render_result(
    html: &mut String,
    state: Option<(&CheckResult, ProblemState)>,
    display: CheckDisplay,
    confirmation_attempts: u32,
) {
    if let Some((result, problem)) = state {
        let class = result.status.label();
        let label = display.label(result.status);
        let _ = write!(html, "<td class=\"{class}\">{label}");
        if let CheckDisplay::Reachability(probe) = display {
            let degraded = if result.status == Status::Warning {
                "degraded; "
            } else {
                ""
            };
            let _ = write!(
                html,
                " ({degraded}{class})<br><small>Latest probe: {}</small>",
                probe.label()
            );
        }
        html.push_str("</td>");
        if let Some(severity) = problem.confirmed_severity {
            let class = severity.label();
            let label = display.label(severity);
            let _ = write!(
                html,
                "<td class=\"{class}\">Confirmed problem: {label}</td>"
            );
        } else {
            html.push_str("<td>No confirmed problem</td>");
        }
        if let Some(severity) = problem.pending_severity {
            let class = severity.label();
            let label = display.label(severity);
            let kind = if problem.confirmed_severity.is_some() {
                "Pending severity change"
            } else {
                "Pending problem"
            };
            let _ = write!(
                html,
                "<td class=\"{class}\">{kind}: {label} ({}/{confirmation_attempts})</td>",
                problem.pending_attempts
            );
        } else {
            html.push_str("<td>No pending problem</td>");
        }
        let _ = write!(
            html,
            "<td><pre>{}</pre></td><td>{}</td><td>{}</td><td>{} ms</td>",
            escape(&result.output),
            escape(&result.started_at),
            escape(&result.finished_at),
            result.duration_ms
        );
    } else {
        html.push_str("<td>Not yet checked</td><td colspan=\"6\">No result available</td>");
    }
    html.push_str("</tr>\n");
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
