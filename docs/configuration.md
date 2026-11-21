# Fleet configuration

Pass a root YAML file with `--config`. Ragios resolves and validates the whole
fleet at startup, before opening SQLite or running plugins. Configuration remains
file-owned; operators can validate it separately and explicitly reload it with
`SIGHUP`. There is no browser editing.

See [`examples/fleet/`](../examples/fleet/) for a runnable, multi-file example.
It runs a local demonstration plugin, not remote probes. The older
[`examples/ragios.yaml`](../examples/ragios.yaml) inline format also works.

## Files and paths

Only the root file sets `bind`, `plugin_dir`, `database`, `interval_seconds`,
`timeout_seconds`, and global `recipients`. The root can also define `commands`,
`bundles`, `groups`, and `hosts`, or split those definitions into fragments:

```yaml
plugin_dir: /usr/lib/nagios/plugins
database: /var/lib/ragios/state.sqlite3
recipients: [operations@example.invalid]
includes: [checks.yaml, hosts/east.yaml, hosts/west.yaml]
```

An included fragment can contain only `commands`, `bundles`, `groups`,
`check_windows`, and `hosts`. Nested includes, globs, directory discovery,
and multiple YAML documents in a file are not supported. Include paths resolve
beside the root YAML file. The same canonical file path cannot be included
twice, including through a symlink or by including the root itself. Missing or
malformed files reject the whole fleet.

Definitions append in root-then-include-list order. References may point forward
or across files; file order never grants override priority. Duplicate command,
bundle, group, or host IDs are errors across the complete configuration.

`plugin_dir` and `database` resolve beside the root file, even when all commands
and hosts come from fragments. Relative executables resolve inside `plugin_dir`;
absolute executables are allowed. Executables are never searched on `PATH`.
Set `database` to a filesystem path, not a SQLite `file:` URI or `:memory:`.
SQLite's parent directory must already exist and be writable.

## Commands, bundles, and hosts

```yaml
commands:
  - id: https-probe
    executable: check_http
    args: ["-H", "{{address}}", "-p", "{{port}}", "-S"]

bundles:
  - id: web
    services:
      - id: https
        name: HTTPS response
        command: https-probe

groups:
  - id: production
    name: Production
    recipients: [production@example.invalid]

hosts:
  - id: web-01
    name: Website
    address: web-01.example.invalid
    group: production
    bundles: [web]
    vars: {port: "443"}
```

A host selects any number of bundles and may also add inline `services`. Each
resolved host must have at least one service. Bundles cannot include other bundles
or inherit templates. A service ID must be unique within a host, across every
assigned bundle and inline service. Even identical definitions collide; use an
explicit override to change a single, unambiguous service instead of redefining
it. The same service ID on different hosts is valid.

Every host and service has a stable `id` and a separate display `name`. Results
are persisted under the pair `(host ID, service ID)`. Renaming display names,
commands, bundles, or groups does not change that pair. Changing an ID creates a
new monitoring identity; moving a service between bundles while retaining its ID
does not. Display names need not be unique.

Services specify exactly one of `command` (a named command ID) or `executable`
(an inline path). Optional service `args` **replace** command arguments, including
an explicit empty list. Named commands contain only `id`, `executable`, and
optional `args`; they do not run checks by themselves.

## Host reachability

A host can optionally configure one reachability probe, independent of its
services. Omission means **reachability not configured**, not reachable or down.
Existing service-only configurations remain valid; hosts still require at least
one service.

```yaml
hosts:
  - id: web-01
    name: Website
    address: web-01.example.invalid
    reachability:
      probe: ping
      executable: check_ping
      args: ["-H", "{{address}}", "-w", "100.0,20%", "-c", "500.0,60%", "-p", "5"]
      interval_seconds: 120
      timeout_seconds: 10
    services:
      - {id: https, name: HTTPS, executable: check_http, args: ["-H", "{{address}}", "-S"]}
```

For TCP, replace the `reachability` block with, for example:

```yaml
    reachability:
      probe: tcp
      executable: check_tcp
      args: ["-H", "{{address}}", "-p", "443"]
```

`probe` must be `ping` or `tcp`; `executable` is required. Use an installed
Nagios-compatible plugin, unchanged, with the arguments its version requires.
`probe` describes the configured measurement; it does not supply an executable
or synthesize arguments. Raw system `ping` commands may use different exit-code
semantics and must not be mistaken for a Nagios-compatible probe. `args` defaults
to `[]`; host placeholders and literal-argument rules below apply. Reachability
uses an explicit `executable`, not a named `command` or service bundle.
Relative executable paths resolve inside `plugin_dir`, including for hosts in
fragments. No implicit shell or PATH lookup is used.

Reachability `interval_seconds` and `timeout_seconds` override the host settings,
then fall back to the root defaults. The same timing validation applies as for
services. Host-level service `overrides` do not affect the reachability probe.
Host and service checks share the global concurrency limit, execution deadlines,
and output capture budget. Each check has its own non-overlapping schedule.

The latest reachability assessment is **reachable** for plugin OK or WARNING,
**down** for CRITICAL, and **undetermined** for UNKNOWN or runner failure.
WARNING is shown as degraded but still reachable. A reachable result clears a
host problem immediately; down and undetermined use the configured confirmation
threshold and retry timing. Mixed initial failures count together; changes to a
confirmed assessment require consecutive matching results at the new assessment.
See the [problem lifecycle](../README.md#problem-lifecycle).

Host results and lifecycle state persist under the host ID in a separate check
namespace from every service ID. Display-name changes preserve state. A TCP
probe and a service may test the same endpoint, but their outcomes remain
independent. Service checks continue while the host is down or undetermined.
Only confirmed down holds service notifications. Confirmed undetermined,
pending failures, and degraded reachability do not. Both confirmed down and
confirmed undetermined can send host reachability email.

Reachability is observed from the monitoring server's network location. A failed
probe does not prove the machine is powered off; a reachable host does not prove
that its services are healthy. Install probe dependencies and grant any required
ping permissions to the dedicated unprivileged server account, not root access
to the Ragios server.

## Literal arguments and host values

Ragios substitutes `{{address}}`, `{{host_id}}`, and keys from a host's string
`vars` map inside argument strings only. Quote numeric variable values, such as
`port: "443"`. `address` is optional unless referenced; `host_id` and `address`
are reserved and cannot be redefined in `vars`. Unknown/unset placeholders and
unclosed `{{...}}` tokens are errors for an assigned check.

Substitution is single-pass: a value containing another placeholder is not
expanded again. One configured argument remains one argument, including when it
contains spaces or shell syntax. Ragios does not expand environment variables,
interpolate executable paths, or evaluate shell commands. Braces other than the
`{{...}}` token syntax are literal. Escape a literal opening `{{` as `{{{{`:
for example, the YAML argument `'{{{{.Names}}'` reaches the plugin as
`{{.Names}}`. This escape also applies to legacy inline arguments that contain
the token syntax. A deliberately configured shell executable can still evaluate
its own arguments; configuration must be trusted.

## Defaults and explicit overrides

The root defaults are `interval_seconds: 300` and `timeout_seconds: 30`.
Hosts and service definitions can each specify those settings. A host can also
override selected effective service fields:

```yaml
hosts:
  - id: web-02
    name: Website
    address: web-02.example.invalid
    group: production
    bundles: [web]
    interval_seconds: 120
    overrides:
      https:
        name: Alternate HTTPS port
        args: ["-H", "{{address}}", "-p", "8443", "-S"]
        interval_seconds: 60
        timeout_seconds: 10
```

For each timing setting, precedence is:

1. Host `overrides.<service ID>` setting.
2. Setting on the service definition.
3. Host-level setting.
4. Root default.

`overrides` is a map keyed by an existing service ID on that host. Its only fields
are `name`, `args`, `interval_seconds`, `timeout_seconds`, `check_window`, and
`notification_window`.
It cannot create or rename an identity, change an executable, or hide a bundle
collision. Argument
overrides replace the entire argument list; `args: []` removes all arguments.
Omitted fields retain their resolved values. Explicit `null` is invalid, not an
instruction to inherit. There are no inherited template chains.

Intervals and deadlines are whole seconds from 1 through 4,294,967,295. All
provided settings are validated, even if a more specific setting would replace
them. Invalid unused commands or bundles also reject the configuration.

## Check windows

A check window is a named recurring weekly period during which checks may
start. Without one, a check runs at any time.

```yaml
check_window: work-hours        # optional root default

check_windows:
  - id: work-hours
    timezone: Europe/London     # IANA name; defaults to UTC
    ranges:
      - {days: [mon, tue, wed, thu, fri], start: "09:00", end: "17:30"}
  - id: weekday-mornings
    ranges:
      - {days: [mon, tue, wed, thu, fri], start: "06:00", end: "07:00"}

hosts:
  - id: web-01
    name: Website
    address: web-01.example.invalid
    check_window: always        # opt out of the root default
    services:
      - id: certificate
        name: Certificate expiry
        executable: check_http
        args: ["-H", "{{address}}", "-C", "30"]
        interval_seconds: 86400
        check_window: weekday-mornings
```

`check_window` follows the same precedence as the timing settings: host
`overrides.<service ID>`, then the service definition, then the host, then the
root. A reachability probe uses its own `check_window`, then the host's, then
the root's. The reserved ID `always` lets a more specific level run at any time;
it cannot be declared.

Each range lists `days` (English names such as `mon` or `Monday`, without
repeats) and local `start` and `end` times as `HH:MM`. A range is open from
`start` up to, but not including, `end`; `end: "24:00"` closes at midnight.
Ranges cannot cross midnight: split an overnight period into two ranges. A
window needs at least one range and can combine any number of them. Holidays
and one-off dates are not supported.

Times are local wall-clock times in the window's timezone, so `09:00` stays
09:00 across daylight-saving changes. A range that starts in a skipped hour opens
when local time resumes; a range in a repeated hour is open during both
occurrences. Stored timestamps and the dashboard remain in UTC.

A window controls when executions start. A check that starts inside its window
runs to completion even if the window closes, and its result is saved normally.
Normal and retry timing are unchanged while the window is open. A check due while
its window is closed, including a retry or a newly added check, runs when the
window next opens. Each check keeps its own interval, so a daily check limited to
`weekday-mornings` runs once each weekday and resumes on Monday after a Friday
run. A closed window does not make a check overdue; see "Restarts and freshness"
in the README.

Unknown window references, duplicate window IDs, invalid timezones, malformed or
empty ranges, and ranges whose start is not before their end reject startup,
validation, and reload, even when nothing uses the window. Window changes apply
on reload without resetting results or problem state.

## Primary groups and recipients

A host's `group` is exactly one group ID, independent of its bundles. The
dashboard orders hosts by group and displays both group names and stable IDs.
For compatibility, hosts that omit `group` belong to the built-in `ungrouped`
group, displayed as **Ungrouped**; that ID is reserved and cannot be declared or
explicitly referenced as a user-defined group.

Recipient selection is:

1. Host `recipients`, if present.
2. Its group's `recipients`, if present.
3. Root `recipients`, defaulting to `[]`.

Each override **replaces**, never merges, the list. An explicit `[]` disables
recipient selection at that level; omission inherits. Recipients must be nonempty
strings and, when email is enabled, each effective recipient must be a bare email
address. Ragios sends one message per distinct recipient address.

## Email delivery

Omit `email` or set `email: {mode: disabled}` for evaluation. To send email through
an authenticated SMTP relay, set the following in the root configuration:

```yaml
reminder_interval_seconds: 3600  # optional; must be positive
email:
  mode: smtp
  server: smtp.example.invalid
  tls: starttls
  from: ragios@example.invalid
  username_file: credentials/smtp-user
  password_file: credentials/smtp-password
```

`tls: starttls` requires a successful TLS upgrade before authentication and
defaults to port 587. Use `tls: implicit` for TLS from connection start (port 465).
Set `port` to override the default. The relay certificate must match `server` and
chain to a trusted root. For a private relay CA, set `ca_file` to its PEM
certificate. Paths resolve relative to the root YAML file unless absolute.

The credential files must be regular files readable by the Ragios account,
with no group or other permissions (for example, `0600` or `0400`). Put one
nonempty UTF-8 line in each file. Do not put the values in YAML or plugin output.
`--validate` checks the settings and files without connecting to the relay.

Confirmed host and service problems and confirmed status changes queue email
in the same SQLite transaction as the check result. Pending-only failures do
not send email. A recovery goes only to still-configured recipients whose
problem message was accepted by the relay during that confirmed problem. A
newer confirmed state supersedes unsent messages.

For each unresolved problem, Ragios schedules a reminder after the current
problem message reaches a delivery outcome (accepted or permanently failed).
The next reminder interval starts when the reminder reaches a delivery outcome.
Changing `reminder_interval_seconds` on reload updates waiting reminders but
does not reschedule messages that are already queued. A restart does not reset
the interval or queue multiple missed reminders.

Confirmed host down holds queued service notifications and reminders while
service checks continue. When confirmed down ends, Ragios reevaluates held
notifications against each service's current confirmed state and what each
recipient already received. Failures that recovered without a delivered problem
message don't send recovery email. While email is disabled, Ragios discards
queued deliveries and cancels reminders; it does not replay disabled transitions.

Open **Email delivery** on the dashboard to see which hosts are holding service
email, which confirmed problems are outside their notification windows, reminder
timing, delivery attempts, and sanitized SMTP diagnostics. Temporary failures
retry starting after three seconds,
with exponential backoff up to five minutes. A `535` authentication rejection
also retries, so pending mail can be delivered after credentials are corrected
and reloaded. Other SMTP 5xx replies are not retried.

### Notification windows

Set `notification_window` to a name from `check_windows` to limit when Ragios can
start an email attempt. This is independent of `check_window`: checks keep
running on their own schedule and their results are saved normally.

```yaml
notification_window: staffed-hours
check_windows:
  - id: staffed-hours
    timezone: America/New_York
    ranges:
      - {days: [mon, tue, wed, thu, fri], start: "09:00", end: "17:00"}
```

`notification_window` uses the same precedence and local-time rules as
`check_window`, with separate assignments. A host, service, service override,
or reachability probe can use `always` to opt out of an inherited restriction.
If no level assigns a window, email can be attempted at any time.

While a window is closed, Ragios holds problems, confirmed severity changes,
recoveries, reminders, and SMTP retries for that check. On reopening it sends
only what still applies. If a problem came and went without a delivered email,
there is nothing to send. If a recipient already knows the current severity,
Ragios resumes reminders instead of reporting an intervening change. If an
already-notified problem recovered and remains recovered, Ragios can send its
recovery. Confirmed host-down suppression still applies to service email, and
at most one overdue reminder is eligible per check and recipient. An SMTP attempt
that has already started can finish after the window closes.
Ragios does not resend a recorded delivery on restart. If the relay accepts a
message but Ragios cannot record that outcome (for example, on a crash), the
outcome is uncertain and a still-current message may be retried. A message
already in flight at a state change or configuration reload may still arrive.

Completed notification episodes are pruned after 30 days without a delivery
update, once all their deliveries are finished. Active episodes keep the records
needed to decide whether to send a recovery.

Run only one Ragios server per SQLite database. Ragios holds an exclusive lock
on a sibling `<database>.ragios.lock` file while running. Leave the lock file in
place; the operating system releases the lock when Ragios exits. Do not expose
the same database through hard links; SQLite's WAL requires a single filename.

## Validation and runtime reload

```sh
ragios --config /etc/ragios/ragios.yaml --validate
kill -HUP <ragios-pid>
```

`--validate` loads and resolves the complete root file and every include. It
prints `Configuration valid` and exits zero on success, or reports diagnostics
and exits nonzero on failure. It does not open SQLite, bind HTTP, or execute
plugins. Validation checks configuration structure and references, not whether
plugins are executable, remote targets are reachable, or SQLite is writable.

Save all files before sending `SIGHUP` to the running server. Ragios rereads its
original root path and includes, validates the complete candidate while existing
checks continue running and saving results, then publishes one resolved
configuration for monitoring and the dashboard. There is no file
watcher, automatic reload, or browser mutation endpoint. Concurrent reload
requests may coalesce; check the server log for the outcome:

- `Configuration reloaded`: the complete candidate is active.
- `Configuration reload rejected: ...`: nothing changed; fix the reported error
  and send `SIGHUP` again.

Unknown YAML fields, duplicate map keys, duplicate IDs, unknown references,
ambiguous command definitions, invalid overrides, empty definitions, and invalid
timing settings reject startup, validation, and reload. Diagnostics identify the
configuration file and the host, service, bundle, group, or command involved as
applicable. Definitions retain their source paths; duplicate IDs report both
files. Parsing an included file also reports its path. Nothing is partially
applied, including when an error is in a later include.

`bind` and the resolved `database` path cannot change at runtime. Changing either
rejects the whole reload with a restart-required diagnostic. Monitoring settings,
plugin paths, commands, bundles, groups, resolved recipients, and SMTP settings
and credential files are reloadable. Queued email to removed recipients or for
removed checks is discarded rather than rerouted.

### Checks and retained state

- New host/service ID pairs become eligible immediately. Removing a pair stops
  scheduling it and hides it from the dashboard, but does not delete its latest
  result, problem state, or lifecycle transitions. Reload adds no pruning of its
  own; transitions follow the normal 30-day retention. Re-adding the pair reuses
  that retained state.
- Display-name changes and group/bundle moves retain state under stable IDs.
  Reload itself never resets confirmation streaks or confirmed problems.
- Executable, argument, plugin-directory, timeout, or output-budget changes make
  the replacement check eligible immediately. The previous latest result stays
  visible until a current execution produces a new one.
- An in-flight execution finishes under its original command, deadline, and
  output budget. If its identity was removed or its execution settings changed,
  its result is discarded without changing persisted state or history. A
  replacement with the same IDs waits for that execution to finish, even after
  removal and re-addition, so executions never overlap themselves.
- Restarting is not a reload: saved checks resume their schedule rather than
  becoming eligible immediately. See "Restarts and freshness" in the README.
- Unchanged in-flight checks can still save results, using the active
  confirmation threshold. Normal/retry interval changes recalculate waiting
  deadlines from the last saved completion; overdue checks become eligible
  without a catch-up queue. Changing confirmation settings affects subsequent
  results, not previously stored state.
- The active global concurrency limit controls new executions. Lowering it lets
  existing executions drain before starting more; increasing it makes additional
  slots available immediately.
