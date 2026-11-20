# Ragios

A Rust monitoring server that runs installed host reachability and service-check
plugins from YAML, stores their latest results in SQLite, and serves an HTML dashboard with HTMX
updates. Email is disabled: this version has no email transport.

## Development setup

Install Devbox and enable the direnv hook for your shell. From the project
root, approve `.envrc` once:

```sh
direnv allow
just
```

Direnv activates Devbox when you enter the project. Devbox provides `just`,
`prek`, and Rustup, which installs the pinned Rust toolchain (including rustfmt
and Clippy) on first use.

Devbox installs the `prek` pre-commit hook on activation if there is no existing
hook or custom `core.hooksPath`. The hook is shared by all Git worktrees. Commit
from the Devbox environment so it can find the Rust tools. The hook checks
staged files for common problems and runs rustfmt and Clippy when relevant Rust
files change. Run `prek run --all-files` to check the entire project.

## Build and run

```sh
just build  # Build the release binary at target/release/ragios
just run    # Run the example server using a debug build
```

Without Devbox, you need Rust 1.90 or newer, Cargo, and a C compiler/linker.
SQLite is compiled into the binary; a system SQLite installation is not
required. Linux and macOS are the supported development targets. Installed
plugins still need their own interpreters, libraries, permissions, and network
access. Run as an unprivileged user, not root.

Open <http://127.0.0.1:8080>. The example executes a local demonstration plugin,
not a real remote probe. The dashboard updates every two seconds; without
JavaScript, reload the page. HTMX is bundled, so no CDN access or frontend build
is needed. Stop with Ctrl-C.

The overview lists every host's checks by group. Filter it by host group and by
problem status (any problem, confirmed problems, or pending problems and severity
changes); filtered addresses such as `/?group=prod&problem=confirmed` can be
shared. Host and check names link to detail pages at `/hosts/{host}`,
`/hosts/{host}/services/{service}`, and `/hosts/{host}/reachability`, with IDs
percent-encoded. A check's detail page shows its escaped output, start and finish
times, duration, result age, and its state changes in chronological order.

## Configuration

`--config` is required. YAML is file-owned; Ragios loads it at startup and on
explicit `SIGHUP` reloads. Validate without starting the server:

```sh
target/release/ragios --config examples/ragios.yaml --validate
```

Validation exits zero on success and nonzero with diagnostics on failure. It
opens no database, binds no listener, and runs no plugins. To reload a running
server after saving all configuration files, use `kill -HUP <ragios-pid>` and
check its log for `Configuration reloaded` or `Configuration reload rejected`.
Invalid reloads leave all active definitions and checks untouched. Runtime
changes to `bind` or `database` require a restart and reject the entire reload.
See [reload semantics](docs/configuration.md#validation-and-runtime-reload).
Unknown keys, malformed YAML, empty definitions, duplicate identities, unknown
references, invalid overrides, and invalid intervals/deadlines fail before any
check starts or the database is opened. IDs identify persisted results; display
names do not.

Fleets can use explicit YAML includes, named commands, reusable service bundles,
primary host groups, and per-host overrides. Recipient selection is host, then
group, then global; overrides replace lists rather than merging them. Email
remains disabled. See [fleet configuration](docs/configuration.md) for the schema,
precedence rules, and validation, and [`examples/fleet/`](examples/fleet/) for a
runnable multi-file example.

The inline format in [`examples/ragios.yaml`](examples/ragios.yaml) remains valid.
Required settings:

- `plugin_dir`: base directory for relative executable paths. Absolute executable
  paths are also allowed. Ragios does not search `PATH` for the executable.
- `database`: SQLite file in a writable directory that already exists. SQLite's
  adjacent WAL/SHM files also need write access. Keep this file across restarts.
- `hosts`: hosts with `id`, `name`, and one or more effective services from
  `bundles` and/or inline `services`. Each inline service has `id`, `name`, either
  `executable` or a named `command`, and an optional `args` list. Hosts may instead
  be declared in explicitly included fragments.

Relative plugin and database paths resolve beside the YAML file, not the current
working directory. Plugin working directories and environment are inherited from
the server. For Python checks, keep the installed script unchanged and make its
shebang/interpreter and dependencies available to the server user.

Optional settings:

| Setting | Default | Meaning |
| --- | --- | --- |
| `bind` | `127.0.0.1:8080` | Explicit HTTP socket address |
| `interval_seconds` | `300` | Delay after a completed check when no confirmation is pending |
| `retry_interval_seconds` | `60` | Delay while an initial problem or severity change awaits confirmation |
| `confirmation_attempts` | `3` | Consecutive results needed to confirm a problem or severity change |
| `timeout_seconds` | `30` | Execution deadline, including output collection and excluding time waiting for a concurrency slot |
| `max_concurrent_checks` | `20` | Global limit on running checks across all hosts |
| `max_output_bytes` | `65536` | Combined stdout/stderr capture budget per execution, in bytes |

Intervals and deadlines are whole seconds between 1 and 4,294,967,295;
`confirmation_attempts` is an integer in the same range. Concurrency and output
limits must be positive whole numbers within the platform's semaphore and buffer
size limits. Invalid limits fail startup validation. Checks become eligible
immediately and wait fairly for a concurrency slot. Each check has at most one
waiting or running execution and never overlaps itself. The normal or retry
interval starts after the result is saved, so missed intervals do not form a
catch-up queue. On restart, stored results and problem state are visible while
new checks run.

For an installed Nagios plugin, set `plugin_dir` to its actual installation
directory, then replace the service definition, for example:

```yaml
      - id: https
        name: HTTPS response
        executable: check_http
        args: ["-H", "example.invalid", "-S", "-p", "443"]
```

Ragios invokes the executable directly. Spaces, `$()`, pipes, and other shell
syntax in arguments remain literal text. Shell behaviour requires deliberately
configuring a shell executable; do not accept command definitions from untrusted
users. Do not put credentials in committed YAML or plugin output.

Hosts can optionally select a ping or TCP reachability probe with its own
executable and argument list. See [host reachability](docs/configuration.md#host-reachability)
for configuration examples. No probe is assumed for existing configurations.
Reachability and service health are displayed and persisted independently;
service checks continue even when a host is down or undetermined. A failed probe
from this monitoring server does not prove that the machine is powered off.

Exit codes 0/1/2/3 appear as OK/WARNING/CRITICAL/UNKNOWN. Missing executables,
permission errors, signal deaths, unexpected exit codes, and timeouts appear as
UNKNOWN with a runner diagnostic, never OK. Standard output and standard error
are captured together up to `max_output_bytes` and escaped as text. Excess output
is drained and discarded until exit or timeout, with an explicit truncation
marker in the result. Stream labels, truncation markers, runner diagnostics, and
UTF-8 replacement characters may add display bytes beyond the raw capture budget.
Truncation does not change a plugin's exit status. Timeout handling kills the
execution process group on Unix and reaps the direct child. These limits are not
a security sandbox for untrusted executables.

## Problem lifecycle

A non-OK result appears immediately as a pending problem. By default, three
consecutive non-OK results confirm it. WARNING, CRITICAL, and UNKNOWN all count
toward that initial streak; the result reaching the threshold determines the
confirmed severity.

After confirmation, changing severity requires `confirmation_attempts`
consecutive results at the same new severity. Changing the candidate severity
restarts that streak at one; returning to the confirmed severity cancels it.
The dashboard shows the latest result, confirmed problem, and pending severity
change separately. Any OK result immediately clears a pending or confirmed
problem. An initial OK result is not a recovery.

Pending problems and pending severity changes use `retry_interval_seconds`.
Confirmation, cancellation of a candidate change, and recovery restore the normal
interval. Delays begin after the result is saved; restarting still checks
immediately.

For host reachability, OK and WARNING mean **reachable**; WARNING is displayed
as degraded but still clears any host problem immediately. CRITICAL means
**down**, and UNKNOWN or a runner failure means **undetermined**. Down and
undetermined assessments share the initial failure streak. After confirmation,
changing between down and undetermined requires the same matching-result streak
as service severity changes. Host problems use the same configurable confirmation
threshold and retry interval, independently of service problems. Only confirmed
down is eligible for future service-notification suppression; notifications are
still disabled.

SQLite saves each result, problem state, and lifecycle transition in one
transaction. Streaks and confirmed severity survive restarts. Transitions include
pending progress, confirmation, candidate changes/cancellation, confirmed severity
changes, and recovery; repeated stable results do not create transitions.
State changes are kept for 30 days: Ragios prunes older ones at startup and
hourly. The latest result and current problem state of every check are never
pruned, so a long-standing confirmed problem stays visible after its
confirmation leaves the history. There is no notification delivery yet.
Databases from before lifecycle support retain their results: an existing non-OK result seeds one pending failure,
not a confirmed problem or invented historical transitions. The host-reachability
schema upgrade preserves existing service results, confirmation state, and
transition IDs; it does not infer host reachability from service results.

## Access and current scope

There are no accounts or browser mutations. Keep the default loopback binding;
private Tailscale access requires an explicit private address in `bind` and
appropriate network policy. Do not expose this unauthenticated server publicly.
The SQLite database and configuration are separate from installed plugins; the
server must be able to read configuration/plugins and write its data directory.

This includes service monitoring ([#1](https://github.com/artran/ragios/issues/1)),
bounded plugin execution ([#2](https://github.com/artran/ragios/issues/2)),
problem confirmation ([#3](https://github.com/artran/ragios/issues/3)), host
reachability ([#4](https://github.com/artran/ragios/issues/4)), fleet
configuration ([#5](https://github.com/artran/ragios/issues/5)), and atomic
configuration reload ([#6](https://github.com/artran/ragios/issues/6)), and
problem and history browsing ([#7](https://github.com/artran/ragios/issues/7)).
Freshness assessment and notifications are later work.
A displayed result is the last observation, not proof of current health. No
existing Nagios server or configuration is modified.

## Checks

```sh
just check  # Formatting, type checking, Clippy, and tests
just test   # Tests only
```

Process/HTTP integration tests require `python3` on `PATH` and use temporary YAML,
SQLite files, and local shell/Python plugins; no live customer endpoints, SMTP
servers, or credentials are involved. They cover not-yet-checked results, all
plugin status codes, literal arguments, inherited environments and Python
dependencies, periodic executions without overlap or catch-up, global concurrency,
bounded output, healthy progress under load, escaped output, startup validation,
timeout/descendant cleanup and error diagnostics, and persisted results observed
through HTTP after restarting the application.
Lifecycle tests cover mixed-severity failure streaks, interrupted candidate
changes, immediate recovery, configurable thresholds/retry timing, and persisted
state and transitions through the store API and rendered dashboard.
Reachability tests cover ping/TCP selection, all exit-status mappings and runner
failures, mixed failure confirmation, immediate degraded recovery, independently
persisted host/service state, legacy database upgrades, shared execution limits,
and continuing service checks while the host is down or undetermined.
Fleet tests also cover multi-file resolution, command/bundle reuse, primary groups,
recipient precedence, host overrides, identity collisions, and display-name changes.
Running-application reload tests cover validation-only operation, add/change/remove,
interval updates, invalid multi-file reload rejection, restart-only settings,
state/history retention, and obsolete in-flight results, including removal and
re-addition of an ID before its old execution finishes.
Browsing tests cover group and problem filtering, no-match states, detail
navigation with percent-encoded IDs, chronological history, hostile output
escaping, and 30-day retention, both against an explicit clock in the store and
end to end with backdated state changes.

## Bundled asset

`assets/htmx.min.js` is HTMX 2.0.8 from
<https://github.com/bigskysoftware/htmx/blob/v2.0.8/dist/htmx.min.js>, embedded in
the binary and served at `/assets/htmx.min.js`.
SHA-256: `22283ef68cb7545914f0a88a1bdedc7256a703d1d580c1d255217d0a50d31313`.
Its license is included in `assets/HTMX-LICENSE`.
