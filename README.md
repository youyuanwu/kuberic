# Kuberic

> **Experimental** — Kuberic is under active development and is not ready for
> production use. The classic v1 stack has been removed. Published v2 images
> and a supported release installation are not available yet.

Kuberic is a stateful replication framework for Kubernetes. The current
level-triggered stack provides the Public V1 replication, copy, history,
session, fencing, and lifecycle contracts plus an experimental namespace-bound
controller and owned-cluster validation harness.

The production control loop initializes fresh agents, originates exact
topology authority, renews restarted process sessions, builds and admits
same-cardinality replacements, and performs delayed failover from observation.
Committed application state remains provider-owned; agent schema 12 stores
history and lifecycle authority.

## Repository

```text
kuberic-runtime/           Application/replicator API, host, protocol, control
kuberic-controller/        CRDs, reconciliation policy and Kubernetes controller
kuberic-level-tests/       Unpublished isolated KinD scenario harness

examples/kvstore2/         Level-triggered conformance application
examples/sqlite/           SQLite Public V1 application
examples/postgres/         PostgreSQL-native Public V1 custom replicator
```

There are exactly two public production crates: `kuberic-runtime` and
`kuberic-controller`. The examples and level tests remain separate,
unpublished workspace packages (`publish = false`).
Applications depend only on `kuberic-runtime`; its default `host` feature supplies
`host::ReplicaHost`. Shared contracts are under `protocol` and `control`.
The `testing` feature provides isolated conformance fixtures without exporting
production capabilities. The SQLite example owns its application-specific WAL
commit-barrier implementation as a private module; it is not a runtime API.
The controller disables runtime default features and owns pure evaluation/plan APIs.

The former `kuberic-core`, `kuberic-operator`, classic KVStore and classic
integration-test packages were removed because they had no active users.
There is no compatibility layer, resource conversion or application-data
migration path.

## Local Validation

Install the pinned Rust toolchain and `protoc`. The repository installer pins
`cargo-nextest` and verifies its archive checksum:

```bash
just install-nextest
cargo check --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --doc --workspace --all-features
just nextest-test
```

PostgreSQL validation requires compatible host-local PostgreSQL binaries and an
unprivileged Linux account:

```bash
just nextest-postgres-smoke
```

The smoke target runs a representative real-PostgreSQL lifecycle selection for
routine local checks. Run `just nextest-postgres` only when the full PostgreSQL
suite is explicitly requested or through its configured CI partitions.

Build once and verify the exact ordinary/PostgreSQL partitions with:

```bash
just nextest-archive
just nextest-validate-archive
```

The complete tier inventory and shard commands are documented in the
[test strategy](docs/features/kuberic/testing.md).

The current test inventory contains 708 tests: 662 ordinary, 41 PostgreSQL,
three KinD, and two build-only.

## Experimental Kubernetes Validation

The retained controller and KVStore2 Dockerfiles are development/CI assets.
They are not published release images.

```bash
export KIND_CLUSTER_NAME=kuberic-level-dev
mkdir -p target/kuberic-level-dev
export KUBECONFIG="$PWD/target/kuberic-level-dev/kubeconfig"
export KUBE_CONTEXT="kind-${KIND_CLUSTER_NAME}"
export KUBERIC_AGENT_BEARER_TOKEN=local-level-triggered-token

just create-kind-cluster
just level-triggered-images
just level-triggered-install
just level-triggered-kind-test bootstrap replacement failover
just delete-kind-cluster
```

Use nondefault owned cluster coordinates. The recipes verify the exact
cluster/context/kubeconfig tuple before every mutation and deletion.

## Applications

### KVStore2

The conformance application exercises the complete level-triggered controller,
agent and runtime path through an HTTP key/value API. Its retained isolated
live scenarios cover fresh bootstrap with process-session renewal,
same-cardinality replacement with preserved data, and sustained-outage
failover with old-primary fencing; the broader behavioral matrix runs in the
cluster-free runtime/provider tiers.

### SQLite

`sqlite-replicated` uses quorum-before-publication WAL-frame replication,
committed snapshots and retained catch-up through Public V1. Fresh replacement
storage is required; prior provider state is rejected unchanged. See the
[SQLite design](docs/features/sqlite/design.md).

### PostgreSQL

`postgres-replicated` is an application-owned custom replicator.
PostgreSQL owns WAL streaming, physical recovery, synchronous policy and
promotion while Kuberic owns generic authority and lifecycle choreography.
It uses the Public V1 lifecycle and owns rejection of its prior provider
formats. See the
[PostgreSQL design](docs/features/postgres/design.md).

## Distribution Status

Classic source and image publication have been removed. Existing external
classic registry artifacts, if still present, are unsupported historical
artifacts.

V2 distribution is deferred. Main-branch and version-tag workflows currently
publish no Kuberic controller or application images. Build from source and use
the isolated development/CI assets above.

## Documentation

- [Level-triggered operator](docs/features/kuberic/level-triggered-operator.md)
- [Stateless default Replicator](docs/features/kuberic/stateless-default-replicator.md)
- [Service Fabric alignment](docs/features/kuberic/service-fabric-alignment.md)
- [Faithful SF V1 replicator port design and plan](docs/features/kuberic/sf-v1-semantic-alignment-plan.md)
- [SF V1 P1 source/scenario ledger](docs/features/kuberic/sf-v1-port-ledger.md) and [provenance/notices](docs/features/kuberic/sf-v1-port-provenance.md)
- [V2 transactional Replicator design](docs/features/kuberic/v2-transactional-replicator-design.md)
- [Replicator boundary and native proof](docs/features/kuberic/replicator-boundary.md)
- [Testing strategy](docs/features/kuberic/testing.md)
- [SQLite design](docs/features/sqlite/design.md)
- [PostgreSQL design](docs/features/postgres/design.md)
- [V1 removal record](docs/proposal/v1-retirement-plan.md)
- [Archived classic architecture](docs/archive/v1/README.md)

Background material under `docs/background/` remains technology reference
material rather than a Kuberic compatibility promise.

The SQL Server observer is maintained separately in
[kuberic-mssql](https://github.com/youyuanwu/kuberic-mssql).

## License

[MIT](LICENSE)
