# Fleet configuration

Pass a root YAML file with `--config`. Ragios resolves and validates the whole
fleet at startup, before opening SQLite or running plugins. Configuration remains
file-owned; there is no reload or browser editing in this version.

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

An included fragment can contain only `commands`, `bundles`, `groups`, and
`hosts`. Nested includes, globs, directory discovery, and multiple YAML documents
in a file are not supported. Include paths resolve beside the root YAML file.
The same canonical file path cannot be included twice, including through a
symlink or by including the root itself. Missing or malformed files reject the whole fleet.

Definitions append in root-then-include-list order. References may point forward
or across files; file order never grants override priority. Duplicate command,
bundle, group, or host IDs are errors across the complete configuration.

`plugin_dir` and `database` resolve beside the root file, even when all commands
and hosts come from fragments. Relative executables resolve inside `plugin_dir`;
absolute executables are allowed. Executables are never searched on `PATH`.
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
are `name`, `args`, `interval_seconds`, and `timeout_seconds`. It cannot create or
rename an identity, change an executable, or hide a bundle collision. Argument
overrides replace the entire argument list; `args: []` removes all arguments.
Omitted fields retain their resolved values. Explicit `null` is invalid, not an
instruction to inherit. There are no inherited template chains.

Intervals and deadlines are whole seconds from 1 through 4,294,967,295. All
provided settings are validated, even if a more specific setting would replace
them. Invalid unused commands or bundles also reject the configuration.

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
strings. The resolved host exposes its group ID and effective recipient list for
notification routing. This version still sends no email and has no recipient
transport configuration.

Do not put SMTP credentials, tokens, or other secrets in YAML or plugin output.
Example recipient addresses use the reserved `.invalid` domain. No environment
or secret expansion is performed by the configuration loader.

## Validation errors

Unknown YAML fields, duplicate map keys, duplicate IDs, unknown references,
ambiguous command definitions, invalid overrides, empty definitions, and invalid
timing settings reject startup. Diagnostics identify the configuration file and
the host, service, bundle, group, or command involved as applicable. Definitions
retain their source paths; duplicate IDs report both files. Parsing an included
file also reports its path. Nothing is partially applied.
