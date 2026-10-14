# kvstore2

`kvstore2` is the unpublished conformance application for Kuberic Public V1.
It implements `StatefulServiceReplica` and `StateProvider`, uses
`DefaultReplicatorFactory`, and delegates process hosting to
`kuberic_runtime::host::ReplicaHost`.

The HTTP API is:

- `PUT /kv/{key}` with a UTF-8 body, returning the committed LSN;
- `GET /kv/{key}`;
- `GET /status` for replica, process-session, role, access, progress, and build
  diagnostics.

Application state lives under `/var/lib/kuberic/application`. Agent schema-12
authority lives separately under `/var/lib/kuberic/.kuberic/agent.sqlite3`.
Prior provider or agent state is rejected unchanged; there is no migration or
import path.

The repository validates the broader lifecycle, history, build, replacement,
failover, fencing, and task behavior in cluster-free runtime/provider tests.
The retained owned-KinD scenarios are:

```bash
just level-triggered-kind-test bootstrap replacement failover
```

Use only an explicitly owned cluster as documented in the
[operator guide](../../docs/features/kuberic/level-triggered-operator.md) and
[test strategy](../../docs/features/kuberic/testing.md).

This example and its images are local/CI assets, not a published release.
