mod email;
mod source;
pub use email::Smtp;
#[cfg(test)]
mod tests;

use anyhow::{Context, Result, ensure};
use std::{
    collections::{BTreeMap, HashSet},
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub bind: SocketAddr,
    pub plugin_dir: PathBuf,
    pub database: PathBuf,
    pub interval_seconds: u64,
    pub timeout_seconds: u64,
    pub max_concurrent_checks: usize,
    pub max_output_bytes: usize,
    pub retry_interval_seconds: u64,
    pub confirmation_attempts: u32,
    pub startup_spread_seconds: u64,
    pub hosts: Vec<Host>,
    pub groups: Vec<HostGroup>,
    pub smtp: Option<Smtp>,
    pub reminder_interval_seconds: u32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Host {
    pub id: String,
    pub name: String,
    pub group: String,
    pub recipients: Vec<String>,
    pub reachability: Option<ReachabilityCheck>,
    pub services: Vec<Service>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct HostGroup {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Service {
    pub id: String,
    pub name: String,
    pub check: Check,
    pub notification_window: Option<CheckWindow>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReachabilityCheck {
    pub probe: Probe,
    pub check: Check,
    pub notification_window: Option<CheckWindow>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Probe {
    Ping,
    Tcp,
}

impl Probe {
    pub fn label(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::Tcp => "tcp",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub interval_seconds: u64,
    pub timeout_seconds: u64,
    /// When the check may start; `None` means at any time.
    pub window: Option<CheckWindow>,
}

/// A named recurring weekly period in local time, shared by check and notification policy.
#[derive(Clone, Debug, PartialEq)]
pub struct CheckWindow {
    pub id: String,
    pub timezone: chrono_tz::Tz,
    pub ranges: Vec<WindowRange>,
}

impl CheckWindow {
    /// Local wall-clock time keeps the window fixed across daylight-saving changes.
    pub fn is_open(&self, at: chrono::DateTime<chrono::Utc>) -> bool {
        let local = at.with_timezone(&self.timezone);
        self.ranges.iter().any(|range| range.contains(&local))
    }
}

impl Host {
    pub fn notification_window(&self, service_id: &str) -> Option<&CheckWindow> {
        if service_id.is_empty() {
            self.reachability.as_ref()?.notification_window.as_ref()
        } else {
            self.services
                .iter()
                .find(|service| service.id == service_id)?
                .notification_window
                .as_ref()
        }
    }

    /// Only restricted targets. An empty service ID denotes host reachability.
    pub fn notification_windows(&self) -> impl Iterator<Item = (&str, &CheckWindow)> {
        self.reachability
            .iter()
            .filter_map(|probe| Some(("", probe.notification_window.as_ref()?)))
            .chain(self.services.iter().filter_map(|service| {
                Some((service.id.as_str(), service.notification_window.as_ref()?))
            }))
    }
}

/// Local time from `start` up to, but excluding, `end` on each of `days`.
#[derive(Clone, Debug, PartialEq)]
pub struct WindowRange {
    pub days: Vec<chrono::Weekday>,
    pub start: TimeOfDay,
    pub end: TimeOfDay,
}

impl WindowRange {
    /// Whether the local date and time fall inside this range.
    pub fn contains(&self, local: &(impl chrono::Datelike + chrono::Timelike)) -> bool {
        self.days.contains(&local.weekday())
            && (self.start..self.end).contains(&TimeOfDay::of(local))
    }

    /// When this range starts on `day`, if it covers that weekday.
    pub fn start_on(&self, day: chrono::NaiveDate) -> Option<chrono::NaiveDateTime> {
        use chrono::Datelike;
        self.days.contains(&day.weekday()).then(|| {
            day.and_time(chrono::NaiveTime::MIN) + chrono::TimeDelta::minutes(self.start.0.into())
        })
    }
}

/// A whole-minute local time written `HH:MM`, from 00:00 to 24:00, where 24:00
/// is the end of the day.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(try_from = "String")]
pub struct TimeOfDay(u32);

impl TimeOfDay {
    pub const END_OF_DAY: Self = Self(24 * 60);

    /// The time on a local clock, truncated to the minute.
    pub fn of(local: &impl chrono::Timelike) -> Self {
        Self(local.hour() * 60 + local.minute())
    }
}

impl TryFrom<String> for TimeOfDay {
    type Error = anyhow::Error;

    fn try_from(text: String) -> Result<Self> {
        text.parse()
    }
}

impl std::str::FromStr for TimeOfDay {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let invalid = || anyhow::anyhow!("{text:?} is not HH:MM from 00:00 to 24:00");
        let (hours, minutes) = text.split_once(':').ok_or_else(invalid)?;
        let field = |field: &str| {
            (field.len() == 2 && field.bytes().all(|b| b.is_ascii_digit()))
                .then(|| field.parse::<u32>().ok())
                .flatten()
        };
        let (hours, minutes) = field(hours).zip(field(minutes)).ok_or_else(invalid)?;
        let time = Self(hours * 60 + minutes);
        ensure!(minutes < 60 && time <= Self::END_OF_DAY, invalid());
        Ok(time)
    }
}

fn index<'a, T>(
    items: &'a [T],
    identity: impl Fn(&T) -> (&String, &Path),
    kind: &str,
) -> Result<BTreeMap<&'a str, &'a T>> {
    let mut result = BTreeMap::new();
    for item in items {
        let (key, source) = identity(item);
        ensure!(
            !key.trim().is_empty(),
            "{kind} ID must not be empty (in {})",
            source.display()
        );
        if let Some(previous) = result.insert(key.as_str(), item) {
            anyhow::bail!(
                "duplicate {kind} ID {key:?} in {} and {}",
                identity(previous).1.display(),
                source.display()
            );
        }
    }
    Ok(result)
}

// Substitute each token once, into one argument. Values are never parsed again or shell-evaluated.
fn expand(arg: &str, host: &source::Host) -> Result<String> {
    let mut rest = arg;
    let mut output = String::new();
    while let Some((prefix, token)) = rest.split_once("{{") {
        output.push_str(prefix);
        if let Some(suffix) = token.strip_prefix("{{") {
            output.push_str("{{");
            rest = suffix;
            continue;
        }
        let (key, suffix) = token
            .split_once("}}")
            .context("unclosed argument placeholder")?;
        let value = match key {
            "host_id" => Some(&host.id),
            "address" => host.address.as_ref(),
            _ => host.vars.get(key),
        }
        .with_context(|| format!("unknown or unset argument placeholder {key:?}"))?;
        output.push_str(value);
        rest = suffix;
    }
    output.push_str(rest);
    Ok(output)
}

pub(crate) fn validate_database_path(path: &Path) -> Result<()> {
    let path = path.as_os_str().as_encoded_bytes();
    ensure!(
        !path.starts_with(b"file:") && path != b":memory:",
        "database must be a filesystem path, not a SQLite URI or :memory:"
    );
    Ok(())
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
        let mut file: source::File = serde_yaml::from_str(&text)
            .with_context(|| format!("parse configuration {}", path.display()))?;
        source::locate(
            path,
            &mut file.commands,
            &mut file.bundles,
            &mut file.groups,
            &mut file.hosts,
            &mut file.check_windows,
        );
        let base = path.parent().unwrap_or(Path::new("."));
        let mut included = HashSet::from([std::fs::canonicalize(path)?]);
        for include in &file.includes {
            ensure!(
                !include.as_os_str().is_empty(),
                "include path must not be empty"
            );
            let include = base.join(include);
            let canonical = std::fs::canonicalize(&include)
                .with_context(|| format!("read included configuration {}", include.display()))?;
            ensure!(
                included.insert(canonical),
                "repeated included configuration {}",
                include.display()
            );
            let text = std::fs::read_to_string(&include)
                .with_context(|| format!("read included configuration {}", include.display()))?;
            let mut fragment: source::Fragment = serde_yaml::from_str(&text)
                .with_context(|| format!("parse included configuration {}", include.display()))?;
            source::locate(
                &include,
                &mut fragment.commands,
                &mut fragment.bundles,
                &mut fragment.groups,
                &mut fragment.hosts,
                &mut fragment.check_windows,
            );
            file.commands.extend(fragment.commands);
            file.bundles.extend(fragment.bundles);
            file.hosts.extend(fragment.hosts);
            file.groups.extend(fragment.groups);
            file.check_windows.extend(fragment.check_windows);
        }
        let mut config = Self::resolve(&file, base)
            .with_context(|| format!("resolve configuration {}", path.display()))?;
        config.plugin_dir = base.join(&config.plugin_dir);
        config.database = base.join(&config.database);
        Ok(config)
    }

    fn resolve(file: &source::File, base: &Path) -> Result<Self> {
        let definitions = source::Definitions {
            commands: index(&file.commands, |c| (&c.id, c.source.as_path()), "command")?,
            windows: index(
                &file.check_windows,
                |w| (&w.id, w.source.as_path()),
                "check window",
            )?,
        };
        file.validate(&definitions)?;
        let resolver = Resolver {
            file,
            commands: definitions.commands,
            bundles: index(&file.bundles, |b| (&b.id, b.source.as_path()), "bundle")?,
            groups: index(&file.groups, |g| (&g.id, g.source.as_path()), "group")?,
            windows: definitions
                .windows
                .into_iter()
                .map(|(id, window)| Ok((id, window.resolve()?)))
                .collect::<Result<_>>()?,
        };
        let hosts = file
            .hosts
            .iter()
            .map(|host| {
                resolver
                    .host(host)
                    .with_context(|| format!("host {:?} in {}", host.id, host.source.display()))
            })
            .collect::<Result<Vec<_>>>()?;
        let mut groups: Vec<_> = file
            .groups
            .iter()
            .map(|group| HostGroup {
                id: group.id.clone(),
                name: group.name.clone(),
            })
            .collect();
        if file.hosts.iter().any(|host| host.group.is_none()) {
            groups.push(HostGroup {
                id: "ungrouped".into(),
                name: "Ungrouped".into(),
            });
        }
        ensure!(
            file.reminder_interval_seconds > 0,
            "reminder_interval_seconds must be positive"
        );
        let smtp = file.email.resolve(base)?;
        if smtp.is_some() {
            for host in &hosts {
                ensure!(
                    host.recipients
                        .iter()
                        .all(|address| address.parse::<lettre::Address>().is_ok()),
                    "invalid SMTP recipient for host {:?}",
                    host.id
                );
            }
        }
        Ok(Self {
            smtp,
            reminder_interval_seconds: file.reminder_interval_seconds,
            bind: file.bind,
            plugin_dir: file.plugin_dir.clone(),
            database: file.database.clone(),
            interval_seconds: file.interval_seconds,
            timeout_seconds: file.timeout_seconds,
            max_concurrent_checks: file.max_concurrent_checks,
            max_output_bytes: file.max_output_bytes,
            retry_interval_seconds: file.retry_interval_seconds,
            confirmation_attempts: file.confirmation_attempts,
            startup_spread_seconds: file.startup_spread_seconds,
            hosts,
            groups,
        })
    }
}

// Raw definitions stay behind this boundary. Runtime consumers see only concrete checks and routing.
struct Resolver<'a> {
    file: &'a source::File,
    commands: BTreeMap<&'a str, &'a source::Command>,
    bundles: BTreeMap<&'a str, &'a source::Bundle>,
    groups: BTreeMap<&'a str, &'a source::Group>,
    windows: BTreeMap<&'a str, CheckWindow>,
}

impl Resolver<'_> {
    /// The first window set in `choices`, most specific first; `always` and
    /// omission at every level leave the check unrestricted.
    fn window<'b>(
        &self,
        choices: impl IntoIterator<Item = Option<&'b String>>,
    ) -> Option<CheckWindow> {
        let id = choices.into_iter().flatten().next()?;
        // Source validation guarantees every reference is defined or reserved.
        (id != source::ALWAYS).then(|| self.windows[id.as_str()].clone())
    }

    fn host(&self, host: &source::Host) -> Result<Host> {
        let group = host
            .group
            .as_ref()
            .map(|id| {
                self.groups
                    .get(id.as_str())
                    .copied()
                    .with_context(|| format!("unknown group {id:?}"))
            })
            .transpose()?;
        let recipients = host
            .recipients
            .as_ref()
            .or_else(|| group.and_then(|g| g.recipients.as_ref()))
            .unwrap_or(&self.file.recipients)
            .clone();
        let mut definitions = Vec::new();
        for id in &host.bundles {
            let bundle = self
                .bundles
                .get(id.as_str())
                .with_context(|| format!("unknown bundle {id:?}"))?;
            definitions.extend(&bundle.services);
        }
        definitions.extend(&host.services);
        ensure!(!definitions.is_empty(), "services must not be empty");
        let identities = index(&definitions, |s| (&s.id, s.source.as_path()), "service")?;
        for id in host.overrides.keys() {
            ensure!(
                identities.contains_key(id.as_str()),
                "override refers to unknown service {id:?}"
            );
        }
        let services = definitions
            .into_iter()
            .map(|service| {
                self.service(host, service).with_context(|| {
                    format!("service {:?} in {}", service.id, service.source.display())
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Host {
            id: host.id.clone(),
            name: host.name.clone(),
            group: host.group.clone().unwrap_or_else(|| "ungrouped".into()),
            recipients,
            reachability: host
                .reachability
                .as_ref()
                .map(|probe| -> Result<_> {
                    Ok(ReachabilityCheck {
                        probe: probe.probe,
                        notification_window: self.window([
                            probe.notification_window.as_ref(),
                            host.notification_window.as_ref(),
                            self.file.notification_window.as_ref(),
                        ]),
                        check: Check {
                            executable: probe.executable.clone(),
                            args: probe
                                .args
                                .iter()
                                .map(|arg| expand(arg, host))
                                .collect::<Result<_>>()?,
                            interval_seconds: probe
                                .interval_seconds
                                .or(host.interval_seconds)
                                .unwrap_or(self.file.interval_seconds),
                            timeout_seconds: probe
                                .timeout_seconds
                                .or(host.timeout_seconds)
                                .unwrap_or(self.file.timeout_seconds),
                            window: self.window([
                                probe.check_window.as_ref(),
                                host.check_window.as_ref(),
                                self.file.check_window.as_ref(),
                            ]),
                        },
                    })
                })
                .transpose()
                .context("reachability check")?,
            services,
        })
    }

    fn service(&self, host: &source::Host, service: &source::Service) -> Result<Service> {
        let overrides = host.overrides.get(&service.id);
        // Source validation guarantees one executable source and valid command references.
        let command = service
            .command
            .as_ref()
            .map(|id| self.commands[id.as_str()]);
        let executable = service
            .executable
            .clone()
            .unwrap_or_else(|| command.unwrap().executable.clone());
        let (args, origin): (&[String], String) =
            if let Some(args) = overrides.and_then(|o| o.args.as_ref()) {
                (args, format!("host override in {}", host.source.display()))
            } else if let Some(args) = &service.args {
                (
                    args,
                    format!("service {:?} in {}", service.id, service.source.display()),
                )
            } else if let Some(command) = command {
                (
                    &command.args,
                    format!("command {:?} in {}", command.id, command.source.display()),
                )
            } else {
                (&[], String::new())
            };
        let args = args
            .iter()
            .map(|arg| expand(arg, host))
            .collect::<Result<Vec<_>>>()
            .with_context(|| format!("arguments from {origin}"))?;
        Ok(Service {
            id: service.id.clone(),
            notification_window: self.window([
                overrides.and_then(|o| o.notification_window.as_ref()),
                service.notification_window.as_ref(),
                host.notification_window.as_ref(),
                self.file.notification_window.as_ref(),
            ]),
            name: overrides
                .and_then(|o| o.name.as_ref())
                .unwrap_or(&service.name)
                .clone(),
            check: Check {
                executable,
                args,
                interval_seconds: overrides
                    .and_then(|o| o.interval_seconds)
                    .or(service.interval_seconds)
                    .or(host.interval_seconds)
                    .unwrap_or(self.file.interval_seconds),
                timeout_seconds: overrides
                    .and_then(|o| o.timeout_seconds)
                    .or(service.timeout_seconds)
                    .or(host.timeout_seconds)
                    .unwrap_or(self.file.timeout_seconds),
                window: self.window([
                    overrides.and_then(|o| o.check_window.as_ref()),
                    service.check_window.as_ref(),
                    host.check_window.as_ref(),
                    self.file.check_window.as_ref(),
                ]),
            },
        })
    }
}
