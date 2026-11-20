# Ragios

A Rust monitoring server that runs installed service-check plugins from YAML,
stores their latest results in SQLite, and serves an HTML dashboard with HTMX
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

## Configuration

`--config` is required. YAML is file-owned; this version loads it at startup.
Unknown keys, malformed YAML, empty definitions, duplicate host IDs, duplicate
service IDs within a host, and invalid intervals/deadlines fail before any
check starts or the database is opened. IDs identify persisted results; display
names do not.

See [`examples/ragios.yaml`](examples/ragios.yaml). Required settings:

- `plugin_dir`: base directory for relative executable paths. Absolute executable
  paths are also allowed. Ragios does not search `PATH` for the executable.
- `database`: SQLite file in a writable directory that already exists. SQLite's
  adjacent WAL/SHM files also need write access. Keep this file across restarts.
- `hosts`: hosts with `id`, `name`, and one or more `services`. Each service has
  `id`, `name`, `executable`, and an optional `args` list.

Relative plugin and database paths resolve beside the YAML file, not the current
working directory. Plugin working directories and environment are inherited from
the server. For Python checks, keep the installed script unchanged and make its
shebang/interpreter and dependencies available to the server user.

Optional settings:

| Setting | Default | Meaning |
| --- | --- | --- |
| `bind` | `127.0.0.1:8080` | Explicit HTTP socket address |
| `interval_seconds` | `300` | Delay after each completed check before its next execution |
| `timeout_seconds` | `30` | Execution deadline, including output collection |

Intervals and deadlines are whole seconds between 1 and 4,294,967,295. Checks
start immediately, run independently, and never overlap themselves. There is no
catch-up queue. On restart, stored results are visible while new checks run.

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

Exit codes 0/1/2/3 appear as OK/WARNING/CRITICAL/UNKNOWN. Missing executables,
permission errors, signal deaths, unexpected exit codes, and timeouts appear as
UNKNOWN with a runner diagnostic, never OK. Standard output and standard error
are retained and escaped as text. Timeout handling kills the execution process
group on Unix and reaps the direct child.

## Access and current scope

There are no accounts or browser mutations. Keep the default loopback binding;
private Tailscale access requires an explicit private address in `bind` and
appropriate network policy. Do not expose this unauthenticated server publicly.
The SQLite database and configuration are separate from installed plugins; the
server must be able to read configuration/plugins and write its data directory.

This is the first service-monitoring slice ([#1](https://github.com/artran/ragios/issues/1)),
not the completed fleet runner. Global concurrency limits and bounded output
capture follow in #2. Problem confirmation, independent host reachability,
reload, history, freshness/overdue assessment, and notifications are later work.
A displayed result is the last observation, not proof of current health. No
existing Nagios server or configuration is modified.

## Checks

```sh
just check  # Formatting, type checking, Clippy, and tests
just test   # Tests only
```

Process/HTTP integration tests use temporary YAML, SQLite files, and local shell
plugins; no live customer endpoints, SMTP servers, or credentials are involved.
They cover not-yet-checked results, all plugin status codes, literal arguments,
periodic executions, escaped output, startup validation, timeout/error diagnostics,
and persisted results observed through HTTP after restarting the application.

## Bundled asset

`assets/htmx.min.js` is HTMX 2.0.8 from
<https://github.com/bigskysoftware/htmx/blob/v2.0.8/dist/htmx.min.js>, embedded in
the binary and served at `/assets/htmx.min.js`.
SHA-256: `22283ef68cb7545914f0a88a1bdedc7256a703d1d580c1d255217d0a50d31313`.
Its license is included in `assets/HTMX-LICENSE`.
