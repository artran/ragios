# Notification policy

`src/store/notifications.rs` owns eligibility and durable hold reconciliation.
Its private `Eligibility` reads configuration and SQLite facts. Suppression changes
belong there, not in the scheduler or mail transport.

The scheduler rejects obsolete execution generations, then passes its current
configuration to `Store::save_with_notifications`. Result, problem, transition,
and Notification writes share the existing SQLite transaction.

## Phases

- **Recording** resolves recipients, Notification windows, and Host group from
  configuration. It records suppression history on deliveries. Entering confirmed
  down Reachability also holds queued, retrying, and in-flight Service deliveries,
  including when the result was saved without email configuration.
- **Selection** skips currently suppressed targets. Queued selection and reminder
  selection share `selection_filter` and one configuration snapshot. A poll queues
  at most one reminder and reconciles at most 32 obsolete deliveries.
- **Dispatch** rechecks configuration identity, current eligibility, and whether
  the Notification still applies. It reconciles held severity changes against
  what each recipient actually received before committing the sending state.
  SMTP runs outside the database lock.

These phases must not become one interchangeable predicate. Recording checks the
Notification window at the observation time; selection and dispatch check it at
their own times. Selection and dispatch suppress Service mail only when the Host
still has configured Reachability and its confirmed state is down. Neither a
pending failure nor confirmed undetermined Reachability qualifies.

An Acknowledgement blocks reminders, including one selected before the
Acknowledgement, but not subsequent confirmed severity changes. Recovery requires
an accepted problem Notification for that recipient. Recording may provisionally
queue Recovery while SMTP is in flight; dispatch requires actual acceptance.

Silence creation and configuration publication call `hold_silenced` under the
database lock before exposing the change. This preserves suppression history even
if group membership changes, SMTP remains busy until after expiry, or Ragios
restarts. `src/store/silences.rs` owns Silence storage and scope validation, not
Notification reconciliation. The schema and SQLite adapter are unchanged.

## Regression tests

`tests/support/smtp.rs` exercises the Store interface with temporary SQLite:
Acknowledgement after selection, Notification window closure before retry,
group-membership reload with a delivery in flight, and restart reconciliation.
The existing TLS transport tests also cover reload during actual SMTP I/O and
recipient-specific Recovery. Tests pass configuration through the same interface
as the scheduler rather than reconstructing suppression policy.
