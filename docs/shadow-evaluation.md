# Compare Ragios with Nagios

Run Ragios alongside Nagios, with email explicitly disabled. Nagios stays the alerting system of record throughout the comparison. All installation, credentials, relay changes, and eventual retirement are operator actions.

## Run the local validation

From the repository root, with Rust and Python 3 installed:

```sh
cargo run --locked -- --config examples/shadow/ragios.yaml --validate
cargo test --locked --test application shadow_
```

The tests start Ragios on loopback with temporary files and local fixture
plugins. They exercise 134 hosts and 537 services, disabled email after a
restart, and a failure-to-recovery scenario with a loopback SMTP relay. They
do not run the site-specific check scripts or contact customer services.
`just check` also runs the fixed-time tests for calendar boundaries and
notification eligibility.

## Prepare the new server

1. Follow the [AL2023 installation guide](install-al2023.md) on the new server. Run its disposable smoke test before using a live fleet. Keep the dashboard private and reserve a separate database for the shadow fleet, such as `/var/lib/ragios/shadow.sqlite3`.
2. Start with the [sanitized shadow YAML](../examples/shadow/). Build a private inventory from the current Nagios checks: host/service mapping, executable and exact arguments, working directory, dependency versions, exit-code contract, schedules, thresholds, timeouts, and effective recipients. Use existing read-only records or exports; do not change Nagios. Keep the inventory and live YAML out of source control.
3. Verify the binaries on the new server, not the old command paths. For packaged checks, find the executable with `rpm -ql` and verify the installed version and flags. For custom checks, use the original trusted script unchanged, check its actual basename and interpreter, and confirm which arguments and credential inputs the script accepts. Confirm that the `ragios` account can execute each installed path (for example, `sudo -u ragios test -x /absolute/path/to/plugin`). A Tailnet check may need a particular node identity and permissions; verify these under the installed service rather than assuming a login shell's access.
4. Confirm that parallel runs are safe: exclude checks with side effects or targets that cannot take the added load. Use credential file references wherever supported. Keep `email: {mode: disabled}` in the root YAML; do not install live SMTP credentials during comparison. If possible, also block outbound SMTP from the shadow server. That does not disable email sent independently by a plugin, so review plugin side effects separately.
5. Install the private YAML and validate as `ragios` (`sudo -u ragios /usr/local/bin/ragios --config /etc/ragios/ragios.yaml --validate`). Start or restart the service if changing the database or listener; otherwise validate then reload. Check the journal, confirm **SMTP email disabled** on **Email delivery**, and confirm fresh results on the overview. Restart once with the same YAML and database and recheck both.

## Compare and record differences

For each representative check, record the two check identities, both timestamps in UTC, both outcomes and diagnostics, expected next runs, and whether a difference is expected. Redact secrets from the record. Compare samples close in time before concluding that a threshold differs.

| Compare | What to verify |
| --- | --- |
| Reachability | Ping/TCP target, port, thresholds, and reachability assessment from each server's network location. Reachability is separate from service health. |
| HTTP/TLS | Scheme, virtual host/SNI, path, redirect policy, certificate thresholds, and any additional certificate trust or name checks. |
| Custom and Tailnet checks | Verified executable and arguments, credential access, effective service identity, JSON field, threshold boundary, or expected version as applicable. |
| Timing and freshness | Normal and retry intervals, deadlines, check-window timezone, weekday certificate runs, and next due times. Distinguish deliberately out-of-window checks from stale results. Check behavior after a restart. |
| Problem handling | Pending versus confirmed status, recovery, and the difference between manual rechecks and scheduled execution. Do not induce failures on customer systems. |
| Notification policy | Effective host/group/global recipients; staffed-hours window; confirmed host-down suppression; active silences and acknowledgements. Look for **Email skipped: disabled** in the check's state history. This records the policy, not a prediction of what SMTP would have sent. Shadow transitions do not queue mail for replay. |

Observe at least one full relevant operating cycle, including a staffed-hours boundary and a weekday/weekend boundary if those windows apply. Resolve unexplained differences before cutover. Keep a dated sign-off and agreed rollback conditions (for example, missed critical checks, persistent stale results, or absent/duplicate alerts).

## Authorize cutover and keep a rollback path

1. Agree who owns alerts at each step and how coverage is maintained while switching. Back up the Ragios YAML and SQLite state using the [installation guide](install-al2023.md); keep a known-good email-disabled YAML copy. Leave Nagios running and unchanged during the comparison. Arrange any later Nagios notification handoff separately with its owner; avoid duplicate paging or an unstaffed gap.
2. Before handing over production recipients, verify delivery using a dedicated relay or operator-only test address and a disposable check. Use the [SMTP configuration reference](configuration.md#email-delivery), with protected username/password files and TLS. Verify the effective recipients (including host and group overrides), a controlled problem and recovery, delivery history, and the notification schedule. Do not assume enabling SMTP will replay past shadow transitions; inspect currently open problems and arrange human handoff for them.
3. Once authorized, apply the reviewed live recipient and SMTP configuration and any outbound SMTP access change. Validate, reload, and check the journal and **Email delivery**. Confirm the actual handoff with the Nagios owner. Keep the old system available for rollback during the agreed observation period.
4. To roll back, restore `email: {mode: disabled}`, validate and reload, and verify the disabled banner. If suppression cannot be verified, stop Ragios and/or block its relay access. An already-started send may still arrive. Confirm that Nagios owns notifications again through the agreed operator handoff; reconcile open incidents manually. Keep Ragios results and logs for diagnosis.
5. Retire Nagios only under a separate approved change, after the observation period and confirmation that the old system is no longer needed for coverage or rollback. Preserve required configs, history, and incident records according to your retention policy.
