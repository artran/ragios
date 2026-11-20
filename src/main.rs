mod monitor;
mod web;

use anyhow::{Context, Result};
use clap::Parser;
use ragios::{config::Config, store::Store};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::watch};

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

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let config = Arc::new(Config::load(&arguments.config)?);
    if arguments.validate {
        println!("Configuration valid: {}", arguments.config.display());
        return Ok(());
    }
    // Install the handler before announcing readiness, so an immediate SIGHUP
    // cannot terminate the process with the OS default action.
    let reloads = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
        .context("listen for SIGHUP")?;
    let store = Store::open(&config.database)?;
    let listener = TcpListener::bind(config.bind)
        .await
        .context("bind HTTP listener")?;
    let (published, active) = watch::channel(config.clone());
    let (status, scheduler) = watch::channel(Default::default());
    eprintln!(
        "Ragios listening on http://{}; email disabled",
        listener.local_addr()?
    );
    let server = axum::serve(listener, web::router(active, scheduler, store.clone()));
    tokio::select! {
        result = server => result.context("HTTP server failed"),
        result = monitor::run(arguments.config, config, store.clone(), published, status, reloads) => result,
        () = prune_history(store) => unreachable!("history pruning never stops"),
        result = tokio::signal::ctrl_c() => result.context("listen for Ctrl-C"),
    }
}

/// Apply state-change retention at startup and hourly. A failed pass is retried
/// on the next tick rather than stopping monitoring.
async fn prune_history(store: Store) {
    let mut ticks = tokio::time::interval(Duration::from_secs(3600));
    loop {
        ticks.tick().await;
        if let Err(error) = store.prune_transitions(chrono::Utc::now()).await {
            eprintln!("prune state-change history: {error:#}");
        }
    }
}
