use super::{Dashboard, WebError, WebResult, escape, layout, load_error, require_mutation};
use axum::{
    Form,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, Uri},
    response::{Html, Redirect},
};
use chrono::{DateTime, SecondsFormat, TimeDelta, Utc};
use ragios::store::{CreateSilence, Silence, SilenceScope};
use serde::Deserialize;
use std::fmt::Write;

const REASON_LIMIT: usize = 500;

#[derive(Deserialize, Default)]
#[serde(default)]
pub(super) struct History {
    expired_page: u32,
}

pub(super) async fn page(
    State(dashboard): State<Dashboard>,
    Query(history): Query<History>,
) -> WebResult {
    let config = dashboard.config.borrow().clone();
    let mut options = String::from(r#"<option value="">Select a scope</option>"#);
    let mut option = |scope: SilenceScope| {
        let value = serde_json::to_string(&scope).expect("silence scope contains only strings");
        let _ = write!(
            options,
            r#"<option value="{}">{}</option>"#,
            escape(&value),
            scope_label(&scope)
        );
    };
    for group in &config.groups {
        option(SilenceScope::Group(group.id.clone()));
    }
    for host in &config.hosts {
        option(SilenceScope::Host(host.id.clone()));
        for service in &host.services {
            option(SilenceScope::Service {
                host: host.id.clone(),
                service: service.id.clone(),
            });
        }
    }
    let csrf = &dashboard.csrf;
    let example = (Utc::now() + TimeDelta::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let expired_page = history.expired_page;
    let fragment = table(&dashboard, expired_page).await?;
    Ok(layout(
        "Ragios — Silences",
        &format!(
            r#"
<nav><a href="/">Overview</a> · <a href="/notifications">Email delivery</a></nav>
<h1>Silences</h1>
<p>A silence holds notifications until its expiry. Checks and problem confirmation continue.</p>
<h2>Create silence</h2>
<form method="post" action="/silences">
<input type="hidden" name="csrf" value="{csrf}">
<label>Scope <select name="scope" required>{options}</select></label>
<label>Expires at (RFC 3339) <input name="expiry" required placeholder="{example}" aria-describedby="expiry-help"></label>
<label>Reason (optional) <input name="reason" maxlength="{REASON_LIMIT}"></label>
<button>Create silence</button>
</form><p id="expiry-help">Include a timezone offset or use UTC, for example <code>{example}</code>.</p>
<div hx-get="/silences/status?expired_page={expired_page}" hx-trigger="every 2s" hx-swap="innerHTML">{fragment}</div>"#
        ),
    ))
}

pub(super) async fn status(
    State(dashboard): State<Dashboard>,
    Query(history): Query<History>,
) -> WebResult {
    Ok(Html(table(&dashboard, history.expired_page).await?))
}

async fn table(dashboard: &Dashboard, expired_page: u32) -> Result<String, WebError> {
    let now = Utc::now();
    let (silences, has_older) = dashboard
        .store
        .silences_at(now, expired_page)
        .await
        .map_err(load_error)?;
    if silences.is_empty() && expired_page == 0 {
        return Ok("<p>No silences recorded.</p>".into());
    }
    let mut html = String::from(
        "<h2>Active and expired silences</h2><div class=\"scroll\"><table><thead><tr><th>Scope</th><th>Expires at (UTC)</th><th>Reason</th><th>Status</th></tr></thead><tbody>",
    );
    for silence in silences {
        let _ = write!(
            html,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
            scope_label(&silence.scope),
            silence
                .expires_at
                .to_rfc3339_opts(SecondsFormat::AutoSi, true),
            escape(silence.reason.as_deref().unwrap_or("—")),
            if silence.expires_at > now {
                "Active"
            } else {
                "Expired"
            },
        );
    }
    html.push_str("</tbody></table></div>");
    if let Some(newer) = expired_page.checked_sub(1) {
        let _ = write!(
            html,
            r#"<a href="/silences?expired_page={newer}">Newer expired silences</a> "#
        );
    }
    if let Some(older) = expired_page.checked_add(1).filter(|_| has_older) {
        let _ = write!(
            html,
            r#"<a href="/silences?expired_page={older}">Older expired silences</a>"#
        );
    }
    Ok(html)
}

fn scope_label(scope: &SilenceScope) -> String {
    match scope {
        SilenceScope::Service { host, service } => {
            format!("Service: {} / {}", escape(host), escape(service))
        }
        SilenceScope::Host(host) => format!("Host: {} (including services)", escape(host)),
        SilenceScope::Group(group) => format!(
            "Host group: {} (including hosts and services)",
            escape(group)
        ),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct SilenceForm {
    csrf: String,
    scope: String,
    expiry: String,
    reason: Option<String>,
}

pub(super) async fn create(
    State(dashboard): State<Dashboard>,
    uri: Uri,
    headers: HeaderMap,
    Form(form): Form<SilenceForm>,
) -> Result<Redirect, WebError> {
    require_mutation(&dashboard, &uri, &headers, &form.csrf)?;
    if uri.query().is_some() {
        return Err((
            StatusCode::BAD_REQUEST,
            "Silences do not accept query parameters.",
        ));
    }
    let scope = serde_json::from_str(&form.scope).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            "Select a service, host, or host group.",
        )
    })?;
    let expires_at = DateTime::parse_from_rfc3339(form.expiry.trim())
        .map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                "Enter an RFC 3339 expiry with a timezone offset.",
            )
        })?
        .with_timezone(&Utc);
    let reason = form
        .reason
        .map(|reason| reason.trim().to_owned())
        .filter(|reason| !reason.is_empty());
    if reason
        .as_ref()
        .is_some_and(|reason| reason.chars().count() > REASON_LIMIT)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "Silence reason must be at most 500 characters.",
        ));
    }
    match dashboard
        .store
        .create_silence(
            Silence {
                scope,
                expires_at,
                reason,
            },
            dashboard.config.clone(),
        )
        .await
        .map_err(load_error)?
    {
        CreateSilence::Created => Ok(Redirect::to("/silences")),
        CreateSilence::InvalidScope => Err((
            StatusCode::BAD_REQUEST,
            "The silence target is not configured.",
        )),
        CreateSilence::InvalidExpiry => Err((
            StatusCode::BAD_REQUEST,
            "Silence expiry must be in the future.",
        )),
    }
}
