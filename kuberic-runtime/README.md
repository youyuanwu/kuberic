# kuberic-runtime

Application and replication runtime for the experimental Public V1 Kuberic
stack.

Together with `kuberic-controller`, this is one of two public production
crates. Examples and level tests are unpublished workspace packages.

```toml
kuberic-runtime = "0.0.1"
```

## Public Boundary

The default `host` feature supplies `host::ReplicaHost`. Shared identities,
commands, reports, and wire conversion live in `protocol` and `control`.
Enable `testing` only for isolated test fixtures. The controller's opt-in
`test-util` feature composes those helpers into the
[downstream in-process harness](../docs/features/kuberic/in-process-testing.md).
Checkout-bound conformance/source-inventory helpers additionally require the
repository-owned `kuberic_workspace_tests` configuration and are not part of
the packaged downstream fixture surface.

The public Service Fabric-shaped interfaces are:

| Interface | Responsibility |
|---|---|
| `StatefulServiceReplica` | application Open, role change, Close, Abort |
| `Replicator` | endpoint lifetime, role/epoch, Close/Abort, progress |
| `PrimaryReplicator` | data loss, configuration, catch-up, build, removal |
| `StateReplicator` | replicate plus copy/replication streams |
| `StateProvider` | provider epoch/progress/copy/data-loss behavior |
| `StatefulServicePartition` | identity, access, factory creation, load/fault reporting |

The protected `Replicator`, `PrimaryReplicator` and `StateProvider` method sets
are unchanged. Assignment and stream APIs intentionally change.

## Built-In Replicator

`DefaultReplicatorFactory::new()` is the sole built-in factory. It constructs the
source-guided Service Fabric classic V1 owners, with no engine persistence
argument, selector, shadow writer, or fallback.

Applications call:

```text
OpenContext.partition
  -> with_factory(DefaultReplicatorFactory)
  -> create_replicator(provider, settings)
  -> ReplicatorInterfaces
```

One creation owns one coherent control/primary/state bundle, one listener, and
at most one provider writer.

## Durable Ownership

- agent schema 13: identity, history, lifecycle/operation authority, terminal
  completion;
- provider: application bytes, progress, retained operations, copy state;
- Replicator memory: queues, peers, ACKs, copy/replication tasks.

Old schemas and supported prior provider formats reject without mutation.
There is no migration or compatibility reader.

Process restart reconstructs native process-local queues from schema-13 authority
and provider state. A cold secondary requires real copy even when it retains
application data. The host reserves a real opaque creation identity before
Open; attachment consumes that same identity. The controller can capture the
exact process/creation session under closed access before survivor epoch barriers,
owned existing-state Open, replacement role replay and exact fresh configuration.
Participant journals advance through validated adjacent stages;
obsolete journals can be superseded only after the accepted topology epoch has
advanced.

## Public V1 Protocol

Protocol version 12 is the only built-in identity. Serialized public values use
the `identity` field. Prior identities and old report/command branches are rejected.

The bounded tonic carrier authenticates with the host's agent bearer credential.
Exact identity, session, epoch, history, storage, configuration, operation, and
build bindings are checked independently at connection and message admission.
This is not TLS, native SF wire interoperability, or a replacement for network
isolation and namespace RBAC.

Agent control uses port 50051. Replication/copy data uses the independently
advertised port 50052. Application traffic is outside both protocol surfaces.

## Custom Replicators

Custom factories use the same public interface bundle and may own their own
transport and storage formats. PostgreSQL is the repository example.

Custom implementations must honor partition access and reject unsupported
provider state. They receive no compatibility adapter for the removed engine.

## Verification

```bash
cargo check -p kuberic-runtime --all-targets --all-features
cargo clippy -p kuberic-runtime --all-targets --all-features -- -D warnings
scripts/check_runtime_public_api.sh
just nextest-test
just nextest-source
```

`begin_replicate` returns the assigned LSN before its separate quorum completion.
Neither that completion nor native progress certifies application commitment.
Services own durable preparation, ordered commitment, visibility and client
success; persisted secondaries durably accept before parameterless ACK.

See [Classic V1 Runtime](../docs/features/kuberic/classic-v1-runtime.md),
[Replicator Boundary](../docs/features/kuberic/replicator-boundary.md), and
[Testing](../docs/features/kuberic/testing.md).
