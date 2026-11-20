# Ragios design

This records decisions agreed during the design interview. The design is ready for final shared-understanding confirmation; implementation has not been authorized.

## Scope

- One monitoring server runs checks against remote targets; no agents or distributed workers.
- The enabled configuration in `/Users/ray/projects/nagios-config/` contains 134 host definitions and 537 service definitions. This is a sizing reference, not a promise to reproduce every Nagios feature.
- Host reachability uses ping or TCP probes. Service health is assessed separately.
- Reuse installed Nagios plugins and the existing custom scripts in `/Users/ray/projects/nagios_checks/main/` unchanged. Commands use an executable and argument list, without implicit shell interpretation.
- Enforce execution deadlines and capture plugin text output and Nagios exit statuses.

## Application and deployment

- Rust server, server-rendered HTML, and HTMX.
- One binary and a SQLite database for latest results, state changes, notification delivery records, silences, and acknowledgements.
- Initial target: Amazon Linux 2023, running as a native systemd service on a new server.
- Run Ragios and plugins under a dedicated unprivileged account. Grant only the permissions needed for reachability probes, not root privileges to the server.
- Keep configuration, plugin locations, and writable data paths explicit so future Nix flake, Docker, or Kubernetes packaging does not require a different application architecture.
- A Kubernetes deployment would remain single-instance with persistent storage, not an active-active monitoring cluster.
- Access is private through Tailscale; no built-in accounts or roles.
- HTTP binds to loopback by default, or an explicitly configured Tailscale address. Protect browser mutations against cross-site requests and escape plugin output as text.
- First-release packaging includes the binary, bundled frontend assets, example configuration, systemd unit, dependency documentation, and installation instructions. Nix, Docker, and Kubernetes packaging are future work.
- Existing Nagios remains untouched on its old server during evaluation and until retirement.

## Configuration

- YAML files are the source of truth for monitoring definitions; the browser cannot edit them.
- Support split configuration files, named commands, reusable service bundles, global defaults, and explicit per-host overrides.
- Avoid chains of inherited templates.
- Each host has one primary group for dashboard organisation and recipient routing, independent of its service bundles.
- Recipient precedence is host override, then group override, then global default. An override replaces rather than merges the recipient list.
- Provide a configuration-validation command and explicit reload without restarting. Validate the complete configuration atomically; an invalid reload leaves the running configuration untouched.
- Hosts and services have stable IDs. Display-name changes preserve history; removing an ID stops its checks.
- Named weekly time windows, with an explicit IANA timezone, independently restrict checks and notifications. No holiday calendar support initially.
- Store timestamps in UTC and default schedules to UTC. Explicit window timezones can follow daylight-saving changes.

## Monitoring and notifications

- Default normal check interval: five minutes. Default retry interval: one minute. Intervals and attempt counts are configurable.
- Default execution limits: 30 seconds per execution, 20 concurrent checks, and 64 KiB of captured output per execution. All are configurable.
- Never overlap executions of the same check or accumulate an unbounded execution backlog.
- Show failures immediately as pending problems.
- By default, confirm a problem after three consecutive failed health assessments: non-OK results for service checks, or down/undetermined results for host reachability checks.
- For service checks, changing between WARNING, CRITICAL, and UNKNOWN does not reset the initial failure streak. The result that reaches the threshold determines the initial confirmed severity.
- After confirmation, require three consecutive results at the same new severity before confirming a severity change, using the configured attempt count rather than hard-coding three.
- A successful result clears the problem immediately.
- For host reachability, plugin OK/WARNING means reachable, CRITICAL means down, and UNKNOWN means undetermined. Show degraded reachability, but only confirmed down suppresses service notifications.
- Continue service checks while a host is down, but suppress service notifications while the host is confirmed down.
- Notify about remaining service problems when the host recovers.
- Email first; Slack later. Default recipients may be overridden per host/group.
- Notify on confirmed WARNING, CRITICAL, UNKNOWN, recovery, and confirmed severity changes.
- Default reminders repeat hourly. No escalation chains or on-call rotations initially.
- When a silence, closed notification window, or host-down suppression ends, notify only about still-current confirmed problems; do not replay obsolete transitions.
- Send recovery notifications only for problems that generated a problem notification.
- An acknowledgement stops reminders, not notifications about subsequent confirmed severity changes.
- Send email through TLS-enabled SMTP without requiring a local mail server. Credentials come from environment variables or protected files, not values embedded in YAML; do not log credential values.
- Deliver notifications independently from checks. Retry temporary delivery failures and discard obsolete queued notifications.
- Preserve confirmed problems across restarts rather than resending every initial alert.
- Distinguish result age and overdue checks from plugin UNKNOWN. Account for intentionally closed check windows when determining whether a check is overdue.
- Spread overdue checks across a short startup period rather than launching them all at once.
- Show scheduler and notification-delivery failures prominently. An external watchdog is required to detect loss of the monitoring server itself.
- Retain state changes for 30 days and the latest result for each check. No performance-data graphing, SLA reports, or long-term metrics initially.

## Operator interface

- Host/service overview, problem filtering, and detail pages showing check output and recent state changes.
- Timed silences suppress notifications without stopping checks. Scope a silence to one service, a host including its services, or a group. Require an expiry and allow an optional reason.
- Persist silences across restarts. Expiring one silence does not override another applicable silence.
- Acknowledging a current problem stops reminders until recovery. An acknowledgement belongs to that host or service problem, not an entire group.
- A check-now action runs only the configured command; no arbitrary command execution or browser-based command editing.
- Manual rechecks bypass check windows but obey concurrency and no-overlap limits. Results participate in normal problem confirmation; notification windows, silences, and acknowledgements still apply.
- Display the latest result separately from the confirmed problem severity while a severity change is pending. Explicitly mark stale results instead of presenting an old successful result as current.

## Evaluation and cutover

- Begin with representative hosts covering the check types actually needed.
- Initially disable email delivery while comparing results against the existing Nagios installation.
- Translate definitions to YAML rather than building a permanent Nagios-configuration compatibility layer.

## Approval boundary

All interview decisions above have been agreed. Present the consolidated design for final shared-understanding confirmation before starting implementation.
