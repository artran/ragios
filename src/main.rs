mod web;

use anyhow::{Context, Result};
use clap::Parser;
use ragios::{
    config::Config,
    runner,
    store::{CheckTarget, Store},
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{net::TcpListener, sync::Semaphore, task::JoinSet};

#[derive(Parser)]
#[command(
    version,
    about = "Monitor host reachability and service health from YAML"
)]
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
        let checks = host
            .reachability
            .iter()
            .map(|probe| (CheckTarget::Host(probe.probe), &probe.check))
            .chain(
                host.services
                    .iter()
                    .map(|service| (CheckTarget::Service(service.id.clone()), &service.check)),
            );
        for (target, check) in checks {
            let host_id = host.id.clone();
            let check = check.clone();
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
                    let result =
                        runner::execute(&config.plugin_dir, &check, config.max_output_bytes).await;
                    drop(permit);
                    let problem = store
                        .save_check(
                            host_id.clone(),
                            target.clone(),
                            result,
                            config.confirmation_attempts,
                        )
                        .await?;
                    let delay = if problem.needs_retry() {
                        config.retry_interval_seconds
                    } else {
                        check.interval_seconds
                    };
                    tokio::time::sleep(Duration::from_secs(delay)).await;
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
