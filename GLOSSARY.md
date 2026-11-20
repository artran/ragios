# Ragios

Ragios monitors host reachability and service health, and notifies operators of problems and recoveries.

## Language

**Host**:
A monitored network target whose reachability is assessed from the monitoring server. A host being reachable does not imply that its services are healthy.

**Service**:
A monitored capability associated with a host, with its own health assessment independent of host reachability.

**Reachability check**:
A probe that assesses whether a host responds from the monitoring server's network location. Failure does not prove that the host is powered off.
_Avoid_: Liveness proof

**Service check**:
A probe that assesses a particular capability and reports OK, WARNING, CRITICAL, or UNKNOWN.

**Check result**:
The outcome of one execution of a reachability check or service check, including its reported status and diagnostic output.

**Pending problem**:
A sequence of unsuccessful check results that has not yet met the configured failure-confirmation threshold.
_Avoid_: Soft state

**Confirmed problem**:
A monitored failure that has met the configured failure-confirmation threshold and remains unresolved. Its confirmed severity can differ from the latest check result while a severity change is awaiting confirmation.
_Avoid_: Hard state

**Reachability**:
Whether a host responds to its configured reachability probe: reachable, down, or undetermined. A degraded response can still establish reachability.

**Recovery**:
A successful health assessment that clears a pending or confirmed problem.

**Notification**:
A message to an operator about a monitored problem or recovery.
_Avoid_: Check result

**Host group**:
The primary organisational grouping of a host, used to browse related hosts and select their default notification recipients. Each host belongs to one host group.

**Service bundle**:
A named collection of service definitions that can be assigned to multiple hosts, independent of their host groups.
_Avoid_: Inheritance chain

**Silence**:
A time-bounded suppression of notifications for selected hosts or services, without stopping their checks.
_Avoid_: Pause monitoring

**Acknowledgement**:
An operator's recognition of a current confirmed problem that stops its reminders until recovery.
_Avoid_: Silence

**Check window**:
A recurring weekly period during which a check is allowed to run.

**Notification window**:
A recurring weekly period during which notifications are allowed to be sent, independent of when checks run.
