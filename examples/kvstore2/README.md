# kvstore2

`kvstore2` is the unpublished conformance application for Kuberic Public V1.
It implements `StatefulServiceReplica` and `StateProvider`, uses
`DefaultReplicatorFactory`, and delegates process hosting to
`kuberic_runtime::host::ReplicaHost`.

Ordinary production Open uses the built-in engine's authenticated sessions and
retained native/application joining. Current host/control/CRD input is matched
format 16. The [M1 format-15 checkpoint](../../docs/features/kuberic/ordinary-hosting-m1.md)
and historical cluster behavior below are not new live or M3 failover evidence.

The HTTP API is:

- `PUT /kv/{key}` with a UTF-8 body, returning the committed LSN;
- `GET /kv/{key}`;
- `GET /status` for replica, process-session, role, access, progress, and build
  diagnostics, including durable operation stage and in-flight instruction.

Application state lives under `/var/lib/kuberic/application`. Agent schema-16
authority lives separately under `/var/lib/kuberic/.kuberic/agent.sqlite3`.
Prior provider or agent state is rejected unchanged; there is no migration or
import path.

The repository validates the broader lifecycle, history, build, replacement,
failover, fencing, and task behavior in cluster-free runtime/provider tests.
The retained pre-M1 owned-KinD scenarios are unsupported at this checkpoint:

```bash
just level-triggered-kind-test bootstrap replacement failover
```

Bootstrap includes a process restart and exact session renewal before the
first quorum write. Replacement preserves committed keys while admitting a new
Pod/storage incarnation. Failover pauses the old primary's containerd task,
waits through the configured delay, proves a new quorum write, resumes the old
primary, and verifies that stale writes remain fenced.

Use only an explicitly owned cluster as documented in the
[operator guide](../../docs/features/kuberic/level-triggered-operator.md) and
[test strategy](../../docs/features/kuberic/testing.md).

This example and its images are local/CI assets, not a published release.
