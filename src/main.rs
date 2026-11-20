mod monitor;
mod web;

use anyhow::{Context, Result};
use clap::Parser;
use ragios::{config::Config, store::Store};
use std::{path::PathBuf, sync::Arc};
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
    eprintln!(
        "Ragios listening on http://{}; email disabled",
        listener.local_addr()?
    );
    let server = axum::serve(listener, web::router(active, store.clone()));
    tokio::select! {
        result = server => result.context("HTTP server failed"),
        result = monitor::run(arguments.config, config, store, published, reloads) => result,
        result = tokio::signal::ctrl_c() => result.context("listen for Ctrl-C"),
    }
}
