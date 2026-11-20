use anyhow::{Context, Result, ensure};
use serde::{
    Deserialize, Deserializer,
    de::{MapAccess, Visitor},
};
use std::{
    collections::BTreeMap,
    net::SocketAddr,
    path::{Path, PathBuf},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct File {
    #[serde(default = "super::default_bind")]
    pub bind: SocketAddr,
    pub plugin_dir: PathBuf,
    pub database: PathBuf,
    #[serde(default = "super::default_interval")]
    pub interval_seconds: u64,
    #[serde(default = "super::default_timeout")]
    pub timeout_seconds: u64,
    #[serde(default = "default_max_concurrent_checks")]
    pub max_concurrent_checks: usize,
    #[serde(default = "default_max_output_bytes")]
    pub max_output_bytes: usize,
    #[serde(default = "default_retry_interval")]
    pub retry_interval_seconds: u64,
    #[serde(default = "default_confirmation_attempts")]
    pub confirmation_attempts: u32,
    #[serde(default = "default_startup_spread")]
    pub startup_spread_seconds: u64,
    #[serde(default)]
    pub commands: Vec<Command>,
    #[serde(default)]
    pub bundles: Vec<Bundle>,
    #[serde(default)]
    pub hosts: Vec<Host>,
    #[serde(default)]
    pub includes: Vec<PathBuf>,
    #[serde(default, deserialize_with = "recipient_list")]
    pub recipients: Vec<String>,
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub check_windows: Vec<Window>,
    #[serde(default, deserialize_with = "non_null")]
    pub check_window: Option<String>,
}

// Fragments contain definitions only; settings and nested includes are deliberately unsupported.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Fragment {
    #[serde(default)]
    pub groups: Vec<Group>,
    #[serde(default)]
    pub commands: Vec<Command>,
    #[serde(default)]
    pub bundles: Vec<Bundle>,
    #[serde(default)]
    pub hosts: Vec<Host>,
    #[serde(default)]
    pub check_windows: Vec<Window>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Window {
    #[serde(skip)]
    pub source: PathBuf,
    pub id: String,
    #[serde(default = "default_timezone", deserialize_with = "non_null_value")]
    pub timezone: String,
    pub ranges: Vec<WindowRange>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WindowRange {
    pub days: Vec<Day>,
    pub start: String,
    pub end: String,
}

#[derive(Clone, Copy, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub(super) enum Day {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl From<Day> for chrono::Weekday {
    fn from(day: Day) -> Self {
        match day {
            Day::Mon => Self::Mon,
            Day::Tue => Self::Tue,
            Day::Wed => Self::Wed,
            Day::Thu => Self::Thu,
            Day::Fri => Self::Fri,
            Day::Sat => Self::Sat,
            Day::Sun => Self::Sun,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Group {
    #[serde(skip)]
    pub source: PathBuf,
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "optional_recipients")]
    pub recipients: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Command {
    #[serde(skip)]
    pub source: PathBuf,
    pub id: String,
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Bundle {
    #[serde(skip)]
    pub source: PathBuf,
    pub id: String,
    pub services: Vec<Service>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Host {
    #[serde(skip)]
    pub source: PathBuf,
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "non_null")]
    pub address: Option<String>,
    #[serde(default, deserialize_with = "non_null")]
    pub group: Option<String>,
    #[serde(default, deserialize_with = "optional_recipients")]
    pub recipients: Option<Vec<String>>,
    #[serde(default, deserialize_with = "non_null")]
    pub interval_seconds: Option<u64>,
    #[serde(default, deserialize_with = "non_null")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, deserialize_with = "non_null")]
    pub check_window: Option<String>,
    #[serde(default, deserialize_with = "unique_map")]
    pub overrides: BTreeMap<String, ServiceOverride>,
    #[serde(default, deserialize_with = "unique_map")]
    pub vars: BTreeMap<String, String>,
    #[serde(default)]
    pub bundles: Vec<String>,
    #[serde(default)]
    pub services: Vec<Service>,
    #[serde(default, deserialize_with = "non_null")]
    pub reachability: Option<Reachability>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reachability {
    pub probe: super::Probe,
    pub executable: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default, deserialize_with = "non_null")]
    pub interval_seconds: Option<u64>,
    #[serde(default, deserialize_with = "non_null")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, deserialize_with = "non_null")]
    pub check_window: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Service {
    #[serde(skip)]
    pub source: PathBuf,
    pub id: String,
    pub name: String,
    #[serde(default, deserialize_with = "non_null")]
    pub command: Option<String>,
    #[serde(default, deserialize_with = "non_null")]
    pub executable: Option<PathBuf>,
    #[serde(default, deserialize_with = "non_null")]
    pub args: Option<Vec<String>>,
    #[serde(default, deserialize_with = "non_null")]
    pub interval_seconds: Option<u64>,
    #[serde(default, deserialize_with = "non_null")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, deserialize_with = "non_null")]
    pub check_window: Option<String>,
}

fn default_timezone() -> String {
    "UTC".into()
}

fn non_null_value<'de, D, T>(deserializer: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    non_null(deserializer).map(Option::unwrap)
}

// Missing means inherit; an explicit null must not silently erase an override.
fn non_null<'de, D, T>(deserializer: D) -> std::result::Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)?
        .map(Some)
        .ok_or_else(|| {
            serde::de::Error::custom("explicit null is not allowed; omit the field to inherit")
        })
}

fn recipient_list<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error> {
    match serde_yaml::Value::deserialize(deserializer)? {
        serde_yaml::Value::Sequence(values) => values
            .into_iter()
            .map(|value| match value {
                serde_yaml::Value::String(value) => Ok(value),
                _ => Err(serde::de::Error::custom("recipient must be a string")),
            })
            .collect(),
        serde_yaml::Value::Null => Err(serde::de::Error::custom(
            "explicit null is not allowed; omit recipients to inherit",
        )),
        _ => Err(serde::de::Error::custom(
            "recipients must be a list of strings",
        )),
    }
}

fn optional_recipients<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Vec<String>>, D::Error> {
    recipient_list(deserializer).map(Some)
}

fn unique_map<'de, D, T>(deserializer: D) -> std::result::Result<BTreeMap<String, T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Unique<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> Visitor<'de> for Unique<T> {
        type Value = BTreeMap<String, T>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a mapping with unique keys")
        }
        fn visit_map<M: MapAccess<'de>>(
            self,
            mut map: M,
        ) -> std::result::Result<Self::Value, M::Error> {
            let mut result = BTreeMap::new();
            while let Some(key) = map.next_key::<String>()? {
                let value = map.next_value::<Option<T>>()?.ok_or_else(|| {
                    serde::de::Error::custom("explicit null is not allowed in a mapping")
                })?;
                if result.contains_key(&key) {
                    return Err(serde::de::Error::custom(format!(
                        "duplicate map key {key:?}"
                    )));
                }
                result.insert(key, value);
            }
            Ok(result)
        }
    }
    struct Checked<T>(BTreeMap<String, T>);
    impl<'de, T: Deserialize<'de>> Deserialize<'de> for Checked<T> {
        fn deserialize<D: Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            deserializer
                .deserialize_map(Unique(std::marker::PhantomData))
                .map(Self)
        }
    }
    Option::<Checked<T>>::deserialize(deserializer)?
        .map(|checked| checked.0)
        .ok_or_else(|| serde::de::Error::custom("explicit null is not allowed for a mapping"))
}

fn default_max_concurrent_checks() -> usize {
    20
}
fn default_max_output_bytes() -> usize {
    64 * 1024
}
fn default_retry_interval() -> u64 {
    60
}
fn default_confirmation_attempts() -> u32 {
    3
}
fn default_startup_spread() -> u64 {
    60
}

fn duration(name: &str, value: u64) -> Result<()> {
    ensure!(
        (1..=u32::MAX as u64).contains(&value),
        "{name} must be between 1 and {}",
        u32::MAX
    );
    Ok(())
}

fn recipients(values: &[String]) -> Result<()> {
    for value in values {
        ensure!(!value.trim().is_empty(), "recipient must not be empty");
    }
    Ok(())
}

// Retain declaration paths through resolution for cross-file diagnostics.
pub(super) fn locate(
    path: &Path,
    commands: &mut [Command],
    bundles: &mut [Bundle],
    groups: &mut [Group],
    hosts: &mut [Host],
    windows: &mut [Window],
) {
    for window in windows {
        window.source = path.into();
    }
    for command in commands {
        command.source = path.into();
    }
    for group in groups {
        group.source = path.into();
    }
    for bundle in bundles {
        bundle.source = path.into();
        for service in &mut bundle.services {
            service.source = path.into();
        }
    }
    for host in hosts {
        host.source = path.into();
        for service in &mut host.services {
            service.source = path.into();
        }
    }
}

/// The reserved window ID that lets a more specific level run at any time.
pub(super) const ALWAYS: &str = "always";

fn window_reference(windows: &BTreeMap<&str, &Window>, id: Option<&String>) -> Result<()> {
    if let Some(id) = id {
        ensure!(
            id == ALWAYS || windows.contains_key(id.as_str()),
            "unknown check window {id:?}"
        );
    }
    Ok(())
}

// Whole minutes of the day as "HH:MM"; 24:00 marks the end of a day.
fn minute_of_day(text: &str, end: bool) -> Option<u32> {
    let (hours, minutes) = text.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let digits = |text: &str| {
        text.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| text.parse::<u32>().ok())?
    };
    let (hours, minutes) = (digits(hours)?, digits(minutes)?);
    let minute = hours * 60 + minutes;
    (minutes < 60
        && if end {
            (1..=24 * 60).contains(&minute)
        } else {
            minute < 24 * 60
        })
    .then_some(minute)
}

impl Window {
    pub fn resolve(&self) -> Result<super::CheckWindow> {
        let resolve = || -> Result<_> {
            ensure!(self.id != ALWAYS, "check window ID always is reserved");
            let timezone = self.timezone.parse().map_err(|_| {
                anyhow::anyhow!(
                    "invalid timezone {:?}; use an IANA name such as Europe/London",
                    self.timezone
                )
            })?;
            ensure!(!self.ranges.is_empty(), "ranges must not be empty");
            let ranges = self
                .ranges
                .iter()
                .map(|range| -> Result<_> {
                    ensure!(!range.days.is_empty(), "days must not be empty");
                    let mut days = Vec::new();
                    for &day in &range.days {
                        let day = chrono::Weekday::from(day);
                        ensure!(!days.contains(&day), "duplicate day {day}");
                        days.push(day);
                    }
                    let start_minute = minute_of_day(&range.start, false)
                        .context("start must be HH:MM from 00:00 to 23:59")?;
                    let end_minute = minute_of_day(&range.end, true)
                        .context("end must be HH:MM from 00:01 to 24:00")?;
                    ensure!(
                        start_minute < end_minute,
                        "start must be before end; split ranges that cross midnight"
                    );
                    Ok(super::WindowRange {
                        days,
                        start_minute,
                        end_minute,
                    })
                })
                .collect::<Result<_>>()?;
            Ok(super::CheckWindow {
                id: self.id.clone(),
                timezone,
                ranges,
            })
        };
        resolve()
            .with_context(|| format!("check window {:?} in {}", self.id, self.source.display()))
    }
}

impl File {
    pub fn validate(
        &self,
        commands: &BTreeMap<&str, &Command>,
        windows: &BTreeMap<&str, &Window>,
    ) -> Result<()> {
        ensure!(
            !self.plugin_dir.as_os_str().is_empty(),
            "plugin_dir must not be empty"
        );
        ensure!(
            !self.database.as_os_str().is_empty(),
            "database must not be empty"
        );
        ensure!(
            !self.hosts.is_empty(),
            "hosts must contain at least one host"
        );
        super::index(
            &self.hosts,
            |host| (&host.id, host.source.as_path()),
            "host",
        )?;
        duration("interval_seconds", self.interval_seconds)?;
        duration("timeout_seconds", self.timeout_seconds)?;
        duration("retry_interval_seconds", self.retry_interval_seconds)?;
        ensure!(
            self.confirmation_attempts > 0,
            "confirmation_attempts must be positive"
        );
        ensure!(
            self.startup_spread_seconds <= u32::MAX as u64,
            "startup_spread_seconds must be between 0 and {}",
            u32::MAX
        );
        ensure!(
            (1..=tokio::sync::Semaphore::MAX_PERMITS).contains(&self.max_concurrent_checks),
            "max_concurrent_checks must be between 1 and {}",
            tokio::sync::Semaphore::MAX_PERMITS
        );
        ensure!(
            (1..=isize::MAX as usize).contains(&self.max_output_bytes),
            "max_output_bytes must be between 1 and {}",
            isize::MAX
        );
        recipients(&self.recipients).context("global recipients")?;
        window_reference(windows, self.check_window.as_ref()).context("global check_window")?;
        for command in &self.commands {
            ensure!(
                !command.executable.as_os_str().is_empty(),
                "command {:?}: executable must not be empty (in {})",
                command.id,
                command.source.display()
            );
        }
        for group in &self.groups {
            ensure!(
                !group.name.trim().is_empty(),
                "group {:?}: name must not be empty (in {})",
                group.id,
                group.source.display()
            );
            ensure!(
                group.id != "ungrouped",
                "group ID ungrouped is reserved (in {})",
                group.source.display()
            );
            if let Some(values) = &group.recipients {
                recipients(values).with_context(|| {
                    format!("group {:?} in {}", group.id, group.source.display())
                })?;
            }
        }
        for bundle in &self.bundles {
            ensure!(
                !bundle.services.is_empty(),
                "bundle {:?}: services must not be empty (in {})",
                bundle.id,
                bundle.source.display()
            );
            super::index(
                &bundle.services,
                |service| (&service.id, service.source.as_path()),
                "service",
            )
            .with_context(|| format!("bundle {:?} in {}", bundle.id, bundle.source.display()))?;
            for service in &bundle.services {
                service.validate(commands, windows).with_context(|| {
                    format!(
                        "bundle {:?}, service {:?} in {}",
                        bundle.id,
                        service.id,
                        service.source.display()
                    )
                })?;
            }
        }
        for host in &self.hosts {
            let validate = || -> Result<()> {
                ensure!(!host.name.trim().is_empty(), "name must not be empty");
                ensure!(
                    !host.vars.contains_key("address") && !host.vars.contains_key("host_id"),
                    "vars cannot redefine address or host_id"
                );
                if let Some(values) = &host.recipients {
                    recipients(values)?;
                }
                window_reference(windows, host.check_window.as_ref())?;
                if let Some(value) = host.interval_seconds {
                    duration("interval_seconds", value)?;
                }
                if let Some(value) = host.timeout_seconds {
                    duration("timeout_seconds", value)?;
                }
                if let Some(probe) = &host.reachability {
                    ensure!(
                        !probe.executable.as_os_str().is_empty(),
                        "reachability: executable must not be empty"
                    );
                    if let Some(value) = probe.interval_seconds {
                        duration("reachability interval_seconds", value)?;
                    }
                    if let Some(value) = probe.timeout_seconds {
                        duration("reachability timeout_seconds", value)?;
                    }
                    window_reference(windows, probe.check_window.as_ref())
                        .context("reachability")?;
                }
                for service in &host.services {
                    service
                        .validate(commands, windows)
                        .with_context(|| format!("service {:?}", service.id))?;
                }
                for (id, overrides) in &host.overrides {
                    if let Some(name) = &overrides.name {
                        ensure!(
                            !name.trim().is_empty(),
                            "override {id:?}: name must not be empty"
                        );
                    }
                    if let Some(value) = overrides.interval_seconds {
                        duration("interval_seconds", value)
                            .with_context(|| format!("override {id:?}"))?;
                    }
                    if let Some(value) = overrides.timeout_seconds {
                        duration("timeout_seconds", value)
                            .with_context(|| format!("override {id:?}"))?;
                    }
                    window_reference(windows, overrides.check_window.as_ref())
                        .with_context(|| format!("override {id:?}"))?;
                }
                Ok(())
            };
            validate()
                .with_context(|| format!("host {:?} in {}", host.id, host.source.display()))?;
        }
        Ok(())
    }
}

impl Service {
    fn validate(
        &self,
        commands: &BTreeMap<&str, &Command>,
        windows: &BTreeMap<&str, &Window>,
    ) -> Result<()> {
        ensure!(!self.id.trim().is_empty(), "service ID must not be empty");
        ensure!(!self.name.trim().is_empty(), "name must not be empty");
        ensure!(
            self.command.is_some() != self.executable.is_some(),
            "specify exactly one of command or executable"
        );
        if let Some(id) = &self.command {
            ensure!(commands.contains_key(id.as_str()), "unknown command {id:?}");
        }
        if let Some(executable) = &self.executable {
            ensure!(
                !executable.as_os_str().is_empty(),
                "executable must not be empty"
            );
        }
        if let Some(value) = self.interval_seconds {
            duration("interval_seconds", value)?;
        }
        if let Some(value) = self.timeout_seconds {
            duration("timeout_seconds", value)?;
        }
        window_reference(windows, self.check_window.as_ref())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ServiceOverride {
    #[serde(default, deserialize_with = "non_null")]
    pub name: Option<String>,
    #[serde(default, deserialize_with = "non_null")]
    pub args: Option<Vec<String>>,
    #[serde(default, deserialize_with = "non_null")]
    pub interval_seconds: Option<u64>,
    #[serde(default, deserialize_with = "non_null")]
    pub timeout_seconds: Option<u64>,
    #[serde(default, deserialize_with = "non_null")]
    pub check_window: Option<String>,
}
