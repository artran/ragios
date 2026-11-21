use anyhow::{Context, Result, ensure};
use lettre::{
    Address, AsyncSmtpTransport, Tokio1Executor,
    transport::smtp::{
        AsyncSmtpTransportBuilder,
        authentication::Credentials,
        client::{Certificate, Tls, TlsParameters},
    },
};
use serde::Deserialize;
use std::{
    fmt,
    fs::OpenOptions,
    io::Read,
    net::IpAddr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum EmailSource {
    #[default]
    Disabled,
    Smtp {
        server: String,
        port: Option<u16>,
        tls: TlsMode,
        from: String,
        username_file: PathBuf,
        password_file: PathBuf,
        ca_file: Option<PathBuf>,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum TlsMode {
    Implicit,
    Starttls,
}

#[derive(Clone, PartialEq)]
pub struct Smtp {
    server: String,
    port: u16,
    tls: TlsMode,
    pub from: Address,
    username: String,
    password: String,
    ca: Option<Vec<u8>>,
}

impl fmt::Debug for Smtp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Smtp { .. }")
    }
}

impl EmailSource {
    pub(super) fn resolve(&self, base: &Path) -> Result<Option<Smtp>> {
        let Self::Smtp {
            server,
            port,
            tls,
            from,
            username_file,
            password_file,
            ca_file,
        } = self
        else {
            return Ok(None);
        };
        ensure!(
            server.parse::<IpAddr>().is_ok() || is_hostname(server),
            "email.server must be a hostname or IP address, not a URL or host:port"
        );
        let port = port.unwrap_or(match tls {
            TlsMode::Implicit => 465,
            TlsMode::Starttls => 587,
        });
        ensure!(port != 0, "SMTP port must not be zero");
        let smtp = Smtp {
            server: server.clone(),
            port,
            tls: *tls,
            from: from
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid SMTP sender address"))?,
            username: secret(&base.join(username_file)).context("SMTP username_file")?,
            password: secret(&base.join(password_file)).context("SMTP password_file")?,
            ca: ca_file
                .as_ref()
                .map(|file| std::fs::read(base.join(file)).context("read SMTP ca_file"))
                .transpose()?,
        };
        // Building the connection pool requires Tokio; validation does not.
        smtp.transport_builder()?;
        Ok(Some(smtp))
    }
}

impl Smtp {
    pub fn transport_builder(&self) -> Result<AsyncSmtpTransportBuilder> {
        let mut tls = TlsParameters::builder(self.server.clone());
        if let Some(pem) = &self.ca {
            tls = tls.add_root_certificate(
                Certificate::from_pem(pem)
                    .map_err(|_| anyhow::anyhow!("invalid SMTP CA certificate"))?,
            );
        }
        let tls = tls
            .build()
            .map_err(|_| anyhow::anyhow!("invalid SMTP TLS settings"))?;
        let tls = match self.tls {
            TlsMode::Implicit => Tls::Wrapper(tls),
            TlsMode::Starttls => Tls::Required(tls),
        };
        Ok(
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&self.server)
                .port(self.port)
                .tls(tls)
                .timeout(Some(Duration::from_secs(10)))
                .credentials(Credentials::new(
                    self.username.clone(),
                    self.password.clone(),
                )),
        )
    }
}

fn is_hostname(server: &str) -> bool {
    let name = server.strip_suffix('.').unwrap_or(server);
    name.len() <= 253
        && name.split('.').all(|label| {
            let bytes = label.as_bytes();
            bytes.len() <= 63
                && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
                && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
                && bytes
                    .iter()
                    .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
        })
}

fn secret(path: &Path) -> Result<String> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK)
        .open(path)
        .context("open credential file")?;
    let metadata = file.metadata().context("inspect credential file")?;
    ensure!(
        metadata.is_file() && metadata.permissions().mode() & 0o077 == 0,
        "credential file must be a regular owner-only file (for example, mode 0600)"
    );
    let mut value = String::new();
    file.take(16 * 1024 + 1)
        .read_to_string(&mut value)
        .context("read credential file")?;
    ensure!(value.len() <= 16 * 1024, "credential file exceeds 16 KiB");
    if value.ends_with('\n') {
        value.pop();
        if value.ends_with('\r') {
            value.pop();
        }
    }
    ensure!(
        !value.is_empty() && !value.contains(['\r', '\n', '\0']),
        "credential file must contain one nonempty line"
    );
    Ok(value)
}
