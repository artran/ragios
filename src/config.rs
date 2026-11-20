use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::HashSet,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    pub plugin_dir: PathBuf,
    pub database: PathBuf,
    #[serde(default = "default_interval")]
    pub interval_seconds: u64,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    pub hosts: Vec<Host>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub id: String,
    pub name: String,
    pub services: Vec<Service>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub id: String,
    pub name: String,
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
}

fn default_bind() -> SocketAddr {
    ([127, 0, 0, 1], 8080).into()
}
fn default_interval() -> u64 {
    300
}
fn default_timeout() -> u64 {
    30
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("read configuration {}", path.display()))?;
        let mut config: Self = serde_yaml::from_str(&text)
            .with_context(|| format!("parse configuration {}", path.display()))?;
        config
            .validate()
            .with_context(|| format!("validate configuration {}", path.display()))?;
        let base = path.parent().unwrap_or(Path::new("."));
        config.plugin_dir = base.join(&config.plugin_dir);
        config.database = base.join(&config.database);
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.plugin_dir.as_os_str().is_empty(),
            "plugin_dir must not be empty"
        );
        ensure!(
            !self.database.as_os_str().is_empty(),
            "database must not be empty"
        );
        ensure!(
            (1..=u32::MAX as u64).contains(&self.interval_seconds),
            "interval_seconds must be between 1 and {}",
            u32::MAX
        );
        ensure!(
            (1..=u32::MAX as u64).contains(&self.timeout_seconds),
            "timeout_seconds must be between 1 and {}",
            u32::MAX
        );
        ensure!(
            !self.hosts.is_empty(),
            "hosts must contain at least one host"
        );
        let mut hosts = HashSet::new();
        for host in &self.hosts {
            ensure!(!host.id.trim().is_empty(), "host ID must not be empty");
            ensure!(hosts.insert(&host.id), "duplicate host ID {:?}", host.id);
            ensure!(
                !host.name.trim().is_empty(),
                "host {:?}: name must not be empty",
                host.id
            );
            ensure!(
                !host.services.is_empty(),
                "host {:?}: services must not be empty",
                host.id
            );
            let mut services = HashSet::new();
            for service in &host.services {
                ensure!(
                    !service.id.trim().is_empty(),
                    "host {:?}: service ID must not be empty",
                    host.id
                );
                ensure!(
                    services.insert(&service.id),
                    "host {:?}: duplicate service ID {:?}",
                    host.id,
                    service.id
                );
                ensure!(
                    !service.name.trim().is_empty(),
                    "service {:?}/{:?}: name must not be empty",
                    host.id,
                    service.id
                );
                ensure!(
                    !service.executable.as_os_str().is_empty(),
                    "service {:?}/{:?}: executable must not be empty",
                    host.id,
                    service.id
                );
            }
        }
        Ok(())
    }
}
