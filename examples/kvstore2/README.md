# kvstore2

`kvstore2` is the unpublished conformance application for Kuberic Public V1.
It implements `StatefulServiceReplica` and `StateProvider`, uses
`DefaultReplicatorFactory`, and delegates process hosting to
`kuberic_runtime::host::ReplicaHost`.

Ordinary production Open uses the built-in engine's authenticated sessions and
retained native/application joining. Current host/control/CRD input is matched
format 16; M1's original format-15 result is historical. Bounded singleton and
unchanged-Primary retained-member recovery preserve acknowledged data through
fresh sessions, nonregressing copy and actual predecessor joins. Real fresh
growth checks copied retained values, exact acceptance and continuing replication.
This is not general M3 election/replacement/handoff qualification; documentation
implementation review and actual independent final gates remain pending.

The HTTP API is:

- `PUT /kv/{key}` with a UTF-8 body, returning the committed LSN;
- `GET /kv/{key}`;
- `GET /status` for replica, process-session, role, access, progress, and build
  diagnostics, including durable operation stage and in-flight instruction.

Application state lives under `/var/lib/kuberic/application`. Agent schema-16
authority lives separately under `/var/lib/kuberic/.kuberic/agent.sqlite3`.
Prior provider or agent state is rejected unchanged; there is no migration or
import path.

The repository qualifies bounded lifecycle, history, copy, ACK, fencing and
actual joining in cluster-free runtime/provider tests. Full owned matching-image
bootstrap (secondary reconstruction and final write/read included) passed on
recovery source `af06768a4e5dbc8ef2a57935e0b12342a2c540eb`, before the later
terminal-owner controller repair at `84ff69cf5790f6b41e82c352fe03c39a40fc7d7a`.
No live pass for that newer controller or complete replacement/failover is
claimed. The development bootstrap reproduction entry point is:

```bash
just level-triggered-kind-test bootstrap
```

Bootstrap includes a process restart and exact session renewal before the
first quorum write, and later reconstructs a secondary before final write/read.
Separate historical replacement/failover bodies remain enabled diagnostics, not
current successful qualification. Replacement intends to preserve keys while
admitting a new Pod/storage incarnation. Failover pauses the old primary's containerd task,
waits through the configured delay, proves a new quorum write, resumes the old
primary, and verifies that stale writes remain fenced.

Use only an explicitly owned cluster as documented in the
[operator guide](../../docs/features/kuberic/level-triggered-operator.md) and
[test strategy](../../docs/features/kuberic/testing.md).

This example and its images are local/CI assets, not a published release.
