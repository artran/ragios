mod config;
mod runner;
mod store;
mod web;

use anyhow::{Context, Result};
use clap::Parser;
use config::Config;
use std::{path::PathBuf, sync::Arc, time::Duration};
use store::Store;
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet};

#[derive(Parser)]
#[command(version, about = "Monitor installed service-check plugins from YAML")]
struct Arguments {
    /// YAML configuration; relative paths inside it resolve beside this file.
    #[arg(long)]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let config = Arc::new(Config::load(&arguments.config)?);
    let store = Store::open(&config.database)?;
    let listener = TcpListener::bind(config.bind)
        .await
        .context("bind HTTP listener")?;
    let mut tasks: JoinSet<Result<()>> = JoinSet::new();
    let executions = Arc::new(Semaphore::new(config.max_concurrent_checks));
    for host in &config.hosts {
        for service in &host.services {
            let host_id = host.id.clone();
            let service = service.clone();
            let config = config.clone();
            let store = store.clone();
            let executions = executions.clone();
            tasks.spawn(async move {
                // One loop per check means at most one waiting/running execution
                // per check, with no interval ticks accumulating behind it.
                loop {
                    let permit = executions
                        .acquire()
                        .await
                        .context("execution limiter closed")?;
                    let result = runner::execute(
                        &config.plugin_dir,
                        &service,
                        config.timeout_seconds,
                        config.max_output_bytes,
                    )
                    .await;
                    drop(permit);
                    store
                        .save(host_id.clone(), service.id.clone(), result)
                        .await?;
                    tokio::time::sleep(Duration::from_secs(config.interval_seconds)).await;
                }
            });
        }
    }
    eprintln!(
        "Ragios listening on http://{}; email disabled",
        listener.local_addr()?
    );
    let server = axum::serve(listener, web::router(config, store));
    tokio::select! {
        result = server => result.context("HTTP server failed"),
        failure = tasks.join_next(), if !tasks.is_empty() => anyhow::bail!("monitoring task stopped: {failure:?}"),
        result = tokio::signal::ctrl_c() => result.context("listen for Ctrl-C"),
    }
}
