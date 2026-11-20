use axum::{
    Router,
    extract::State,
    http::{StatusCode, header},
    response::Html,
    routing::get,
};
use ragios::{config::Config, store::Store};
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
<title>Ragios — service health</title><script src="/assets/htmx.min.js" defer></script>
<style>
body{{font:1rem system-ui,sans-serif;margin:2rem;background:#f5f7fa;color:#152336}}
main{{max-width:90rem;margin:auto}}table{{width:100%;border-collapse:collapse;background:white}}
th,td{{padding:.7rem;text-align:left;border-bottom:1px solid #d8dee8;vertical-align:top}}
pre{{white-space:pre-wrap;overflow-wrap:anywhere;margin:0;max-width:40rem}}
.OK{{color:#146138}}.WARNING{{color:#805000}}.CRITICAL{{color:#b02020}}.UNKNOWN{{color:#654398}}
small{{color:#45556b}}.scroll{{overflow-x:auto}}
</style></head><body><main><h1>Ragios</h1><p>Service health · email disabled</p>
<p>Latest results, confirmed problems, and pending changes are shown separately. Host reachability is not yet assessed.</p>
<div id="status" hx-get="/status" hx-trigger="every 2s" hx-swap="innerHTML">{fragment}</div>
<noscript>Reload this page to refresh results.</noscript></main></body></html>"#
    )))
}

async fn status(State(dashboard): State<Dashboard>) -> WebResult {
    Ok(Html(render(&dashboard).await?))
}

async fn render(dashboard: &Dashboard) -> Result<String, (StatusCode, &'static str)> {
    let latest = dashboard.store.latest().await.map_err(|error| {
        eprintln!("load dashboard results: {error:#}");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Cannot load monitoring results; consult the server log.",
        )
    })?;
    let mut html = String::from(
        "<div class=\"scroll\"><table><thead><tr><th>Group</th><th>Host</th><th>Service</th><th>Latest result</th><th>Confirmed problem</th><th>Pending problem / severity change</th><th>Output</th><th>Started (UTC)</th><th>Finished (UTC)</th><th>Duration</th></tr></thead><tbody>",
    );
    for group in &dashboard.config.groups {
        for host in dashboard
            .config
            .hosts
            .iter()
            .filter(|host| host.group == group.id)
        {
            for service in &host.services {
                let _ = write!(
                    html,
                    "<tr><td>{}<br><small>{}</small></td><td>{}<br><small>{}</small></td><td>{}<br><small>{}</small></td>",
                    escape(&group.name),
                    escape(&group.id),
                    escape(&host.name),
                    escape(&host.id),
                    escape(&service.name),
                    escape(&service.id)
                );
                if let Some(state) = latest.get(&(host.id.clone(), service.id.clone())) {
                    let result = &state.result;
                    let label = result.status.label();
                    let _ = write!(html, "<td class=\"{label}\">{label}</td>");
                    if let Some(severity) = state.problem.confirmed_severity {
                        let label = severity.label();
                        let _ = write!(
                            html,
                            "<td class=\"{label}\">Confirmed problem: {label}</td>"
                        );
                    } else {
                        html.push_str("<td>No confirmed problem</td>");
                    }
                    if let Some(severity) = state.problem.pending_severity {
                        let label = severity.label();
                        let kind = if state.problem.confirmed_severity.is_some() {
                            "Pending severity change"
                        } else {
                            "Pending problem"
                        };
                        let _ = write!(
                            html,
                            "<td class=\"{label}\">{kind}: {label} ({}/{})</td>",
                            state.problem.pending_attempts, dashboard.config.confirmation_attempts
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
                    html.push_str(
                        "<td>Not yet checked</td><td colspan=\"6\">No result available</td>",
                    );
                }
                html.push_str("</tr>\n");
            }
        }
    }
    html.push_str("</tbody></table></div>");
    Ok(html)
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
