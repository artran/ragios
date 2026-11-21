# Install Ragios on Amazon Linux 2023

Use a new AL2023 machine. These instructions do not copy or edit Nagios
configuration, stop Nagios, or enable email. The example checks only the new
machine. Run the commands from the root of the Ragios checkout unless noted.

## Build a release

Use a builder running the same AL2023 release and CPU architecture (`aarch64`
or `x86_64`) as the target. Check out an exact Ragios tag or commit and install
Rustup using the [official installer](https://rustup.rs/) as your build user.
The repository's `rust-toolchain.toml` pins the Rust toolchain; `Cargo.lock`
pins the dependency graph. Record the AL2023 release used by DNF and keep the
builder on that version for repeat builds; this is not a bit-for-bit build
recipe.

```sh
sudo dnf install gcc gcc-c++ make git
rpm -q system-release
git rev-parse HEAD
rustc --version
cargo build --locked --release
ldd target/release/ragios
(cd target/release && sha256sum ragios > ragios.sha256)
```

The release artifact is `target/release/ragios`. HTMX and SQLite are compiled
in; no Node.js build, asset directory, or system SQLite is needed. This is a
native GNU/Linux binary, not a fully static build. Keep the AL2023 dynamic
loader/glibc and `libgcc` runtime installed and use `ldd` to check the exact
artifact on the target (no `not found` lines). Use `ca-certificates` for
plugins that use the system TLS trust store and `tzdata` for system timezone
data. Plugin RPMs and Python scripts have their own dependencies. Transfer the
binary and checksum from the trusted builder, and use the unit, example YAML,
and smoke test from the same source revision. Verify the checksum beside the
binary before installation with `sha256sum -c ragios.sha256`.

## Install plugins and create the service account

AWS lists `nagios-plugins-http`, `nagios-plugins-tcp` and `nagios-plugins-ping`
for both architectures in the [AL2023.12 package inventory][packages]. Use
that or a newer supported AL2023 release with these packages available in
[AWS's SPAL repository][spal]; do not add a Fedora or EPEL repository. Confirm
availability in the host's locked repository version before installing. If
`dnf repoquery` does not find the packages, check `dnf repolist` and the AWS
release instructions before proceeding.

```sh
sudo dnf install spal-release
sudo dnf repoquery nagios-plugins-http nagios-plugins-tcp nagios-plugins-ping
sudo dnf install nagios-plugins-http nagios-plugins-tcp nagios-plugins-ping iputils
sudo dnf install ca-certificates tzdata curl

sudo groupadd --system ragios
sudo useradd --system --gid ragios --home-dir /var/lib/ragios \
  --shell /sbin/nologin --no-create-home ragios
sudo install -d -o root -g ragios -m 0750 /etc/ragios /etc/ragios/credentials
sudo install -d -o root -g root -m 0755 /usr/local/libexec/ragios/plugins

for check in http tcp ping; do
  path=$(rpm -ql "nagios-plugins-$check" | grep -E "/check_$check$")
  test -x "$path" || break
  sudo ln -s "$path" "/usr/local/libexec/ragios/plugins/check_$check" || break
done
```

The RPM query discovers the installed architecture-specific executable paths.
Check that all three links were created; the example uses those names instead
of assuming an RPM layout. Add more packaged checks using `dnf repoquery` and
`rpm -ql` in the same way. Keep installed plugins and their parent directories
root-owned and not writable by `ragios`.

### Probe permissions

Ordinary outbound TCP connections (`check_tcp` and `check_http`) need no
capabilities. Nagios `check_ping` runs the packaged `ping` from `iputils`.
Use Linux ICMP echo datagram sockets rather than a raw-socket helper. On the
new dedicated server, allow only the Ragios group through
[`net.ipv4.ping_group_range`][icmp]:

```sh
ragios_gid=$(id -g ragios)
printf 'net.ipv4.ping_group_range = %s %s\n' "$ragios_gid" "$ragios_gid" \
  | sudo tee /etc/sysctl.d/90-ragios-ping.conf >/dev/null
sudo sysctl -p /etc/sysctl.d/90-ragios-ping.conf
sysctl net.ipv4.ping_group_range
```

This setting is host-wide; narrowing the range removes datagram ping access
for other groups. Check for competing sysctl files and verify the range after
reboot. Keep `NoNewPrivileges=yes` and the empty capability bounding set in
`ragios.service`: neither the server nor its plugins can gain privileges via
setuid or file capabilities. Do not add `CAP_NET_RAW` to Ragios, or substitute
`check_icmp` (a raw-socket plugin) for `check_ping`. If the packaged ping cannot
use a datagram socket on the target, inspect the plugin diagnostic and use a
TCP reachability probe on a known listening port instead of granting broad
permissions. The installed-unit smoke test checks actual ping and TCP results
with the capability restrictions in effect.

### Existing Python checks and secrets

Install the custom check unchanged, with mode `0755`, owned by `root:root`,
under `/usr/local/libexec/ragios/plugins`, or use a trusted absolute executable
path in YAML. Install the Python version required by its shebang on the
monitoring server. `ProtectHome=yes` hides home directories from the unit;
put scripts, interpreters and dependencies outside them. Commands run with
`/var/lib/ragios` as their working directory, not the script's directory;
configure absolute paths for dependency files and credentials.

Install checked, version-pinned dependencies into a dedicated root-owned
virtual environment (install `python3` and `python3-pip` with DNF for the
system Python). For example, create `/opt/ragios/venv` with
`sudo python3 -m venv /opt/ragios/venv`, then install from the check's reviewed
lock file with `sudo /opt/ragios/venv/bin/python -m pip install -r requirements.txt`.
For scripts with an `#!/usr/bin/env python3` shebang, set
`PATH=/opt/ragios/venv/bin:/usr/local/bin:/usr/bin` in `/etc/ragios/plugin.env`.
For scripts with a fixed interpreter shebang, provide that exact interpreter
and its dependencies, or configure the absolute venv Python as the executable
and the unchanged script path as the first argument. Add the check to the
installed service's YAML and inspect its dashboard result to verify imports,
file access, and exit status under systemd rather than in your login shell.

Prefer file-based credentials. For custom checks that accept credential files,
install them under `/etc/ragios/credentials` with owner `root:ragios` and mode
`0640`. Pass the **path**, not the secret, to the check.

SMTP credentials have stricter permissions. Set the YAML `username_file` and
`password_file` keys to regular files owned by `ragios:ragios` with mode `0400`
or `0600`. Keep the parent directory owned by `root:ragios` with mode `0750`.
Ragios rejects SMTP files with any group or other permissions, including `0640`.
See the [email configuration reference](configuration.md#email-delivery).

For a legacy script that only accepts environment variables, make
`/etc/ragios/plugin.env` root-owned and mode `0600`. Systemd reads it before
starting the unprivileged process. Ragios and all its plugins inherit the
values, so only run trusted checks; environment values are not isolated
secrets. Do not put secrets in the unit, command arguments, plugin output,
source control, or the examples. Use file references where possible.

## Install and start the unit

The following commands are for a fresh install. On upgrades, do not overwrite
your working YAML or credentials with the example. Keep a backup of them and
the SQLite database before replacing the binary.

```sh
sudo install -o root -g root -m 0755 target/release/ragios /usr/local/bin/ragios
sudo install -o root -g ragios -m 0640 deploy/al2023/ragios.yaml /etc/ragios/ragios.yaml
sudo install -o root -g root -m 0644 deploy/al2023/ragios.service /etc/systemd/system/ragios.service
sudo -u ragios /usr/local/bin/ragios --config /etc/ragios/ragios.yaml --validate
sudo systemd-analyze verify /etc/systemd/system/ragios.service
sudo systemctl daemon-reload
```

On a fresh test server, run the smoke test **before** first startup:

```sh
sudo bash deploy/al2023/smoke-test.sh
```

The script starts and restarts the installed unit. It checks that the process
runs without capabilities, the HTTP and TCP service checks and ping reachability
are healthy, the served HTMX matches the source asset, and the same results are
visible after restart. It refuses non-example YAML or an existing database and
leaves the unit running on success. Repeat on a fresh disposable AL2023 host or
revert the test VM to a pre-start snapshot; never clear production state for
this test. On failure, inspect `sudo journalctl -u ragios -b` and the check's
output on the dashboard. A start failure or plugin permission error is not a
passing result.

Enable monitoring on boot and inspect its logs:

```sh
sudo systemctl enable --now ragios.service
sudo journalctl -u ragios.service -b
```

`StateDirectory=ragios` makes `/var/lib/ragios` writable by the service and
persistent across restarts. Keep the SQLite file and its WAL/SHM sidecars on
that filesystem. Back up SQLite using its backup API or with the unit stopped;
do not copy only a live database file. Validate YAML as `ragios` before
`sudo systemctl reload ragios.service`, then check the journal for acceptance.
Changes to the bind address, database path, or `plugin.env` require a restart.

## Access from a private machine

Keep `127.0.0.1:8080` unless remote access is required. For direct Tailscale
access, install Tailscale separately and set `bind` to the server's explicit
Tailscale IP and port, not `0.0.0.0`. Update or replace the local dashboard
checks if they no longer match the listener. Ensure the tailnet IP exists
before startup, and use tailnet policy and firewall rules to allow access only
from approved operator devices. Use the exact bound IP and port in the browser
(Tailscale DNS names are not accepted as HTTP Host values). Never allow this
port in public ingress rules and do not publish it through a public proxy or
Tailscale Funnel.

Ragios does not have accounts. Anyone with allowed network access has dashboard
access, including mutations. Manual check and silence actions use POST with a
dashboard token to protect against cross-site requests, and Ragios rejects
unexpected HTTP Host values. Those checks do not replace private network access
control.

The application does not depend on systemd packaging. If deployed on Kubernetes
later, use one instance with a persistent volume for SQLite, not multiple
replicas sharing the file. Run an external watchdog on a different machine to
detect total monitoring-server loss; a local restart policy or self-check
cannot detect loss of the whole host from outside.

[packages]: https://docs.aws.amazon.com/linux/al2023/release-notes/all-packages-AL2023.12.html
[spal]: https://docs.aws.amazon.com/linux/al2023/ug/configure-spal-repository.html
[icmp]: https://docs.kernel.org/networking/ip-sysctl.html#icmp-settings
