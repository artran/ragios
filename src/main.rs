mod mail;
mod monitor;
mod web;

use anyhow::{Context, Result};
use clap::Parser;
use ragios::{config::Config, store::Store};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch},
};

#[derive(Parser)]
#[command(
    version,
    about = "Monitor host reachability and service health from YAML"
)]
struct Arguments {
    /// YAML configuration; relative paths inside it resolve beside this file.
    #[arg(long)]
    config: PathBuf,
    /// Validate the complete configuration without opening SQLite or starting checks.
    #[arg(long)]
    validate: bool,
}

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(run(Arguments::parse()));
    // A system DNS lookup may still be running on Tokio's blocking pool.
    // Delivery has already had its chance to record a known SMTP outcome.
    runtime.shutdown_timeout(Duration::from_secs(1));
    result
}

async fn run(arguments: Arguments) -> Result<()> {
    let config = Arc::new(Config::load(&arguments.config)?);
    if arguments.validate {
        println!("Configuration valid: {}", arguments.config.display());
        return Ok(());
    }
    // Install signal handlers before announcing readiness.
    use tokio::signal::unix::{SignalKind, signal};
    let reloads = signal(SignalKind::hangup()).context("listen for SIGHUP")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("listen for SIGINT")?;
    let mut terminate = signal(SignalKind::terminate()).context("listen for SIGTERM")?;
    let store = Store::open_server(&config.database)?;
    let listener = TcpListener::bind(config.bind)
        .await
        .context("bind HTTP listener")?;
    let (published, active) = watch::channel(config.clone());
    let (status, scheduler) = watch::channel(Default::default());
    let (request_check, requests) = mpsc::channel(1);
    let address = listener.local_addr().context("read bound HTTP address")?;
    let router = web::router(
        active.clone(),
        scheduler,
        store.clone(),
        request_check,
        address,
    )?;
    let email = if config.smtp.is_some() {
        "enabled"
    } else {
        "disabled"
    };
    eprintln!("Ragios listening on http://{address}; email {email}");
    let server = axum::serve(listener, router);
    let mut delivery = std::pin::pin!(mail::run(store.clone(), active));
    let result = tokio::select! {
        result = server => result.context("HTTP server failed"),
        result = monitor::run(arguments.config, config, store.clone(), published, status, reloads, requests) => result,
        () = &mut delivery => return Ok(()),
        () = prune_history(store) => unreachable!("history pruning never stops"),
        _ = interrupt.recv() => Ok(()),
        _ = terminate.recv() => Ok(()),
    };
    // The monitor's config sender is now dropped. Let an in-flight SMTP
    // attempt finish and record its outcome, without claiming another email.
    if tokio::time::timeout(Duration::from_secs(20), delivery)
        .await
        .is_err()
    {
        eprintln!("Timed out waiting for email delivery to stop");
    }
    result
}

/// Apply state-change retention at startup and hourly. A failed pass is retried
/// on the next tick rather than stopping monitoring.
async fn prune_history(store: Store) {
    let mut ticks = tokio::time::interval(Duration::from_secs(3600));
    loop {
        ticks.tick().await;
        let now = chrono::Utc::now();
        if let Err(error) = store.prune_transitions(now).await {
            eprintln!("prune state-change history: {error:#}");
        }
        if let Err(error) = store.prune_notifications(now).await {
            eprintln!("prune notification history: {error:#}");
        }
    }
}
