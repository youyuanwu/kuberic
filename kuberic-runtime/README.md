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

The protected method sets are unchanged by the Public V1 replacement.

## Built-In Replicator

`DefaultReplicatorFactory` is the sole built-in factory. It constructs the
stateless Public V1 engine; there is no legacy engine, selector, preview mode,
or fallback.

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

- agent schema 12: identity, history, lifecycle/operation authority, terminal
  completion;
- provider: application bytes, progress, retained operations, copy state;
- Replicator memory: queues, peers, ACKs, copy/replication tasks.

Old schemas and supported prior provider formats reject without mutation.
There is no migration or compatibility reader.

Process restart reconstructs the built-in engine from schema-12 authority and
provider state. A contained process first reopens existing state, then the
controller renews the exact process/creation session through access closure,
survivor epoch barriers, replacement role replay, and exact fresh
configuration. Participant journals advance through validated adjacent stages;
obsolete journals can be superseded only after the accepted topology epoch has
advanced.

## Public V1 Protocol

Protocol version 11 is the only built-in identity. Serialized public values use
the `identity` field. Protocol 10 and old report/command branches are rejected.

Replication transport has no authentication contract. Exact identity,
session, epoch, history, storage, configuration, operation, and build values
provide correctness fencing.

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

See [Stateless Default Replicator](../docs/features/kuberic/stateless-default-replicator.md),
[Replicator Boundary](../docs/features/kuberic/replicator-boundary.md), and
[Testing](../docs/features/kuberic/testing.md).
