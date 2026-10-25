# Kuberic

> **Experimental** — Kuberic is under active development and is not ready for
> production use. The retired Kuberic classic application/operator stack has been removed. Published v2 images
> and a supported release installation are not available yet.

Kuberic is a stateful replication framework for Kubernetes. The current
level-triggered stack provides the Public V1 replication, copy, history,
session, fencing, and lifecycle contracts plus an experimental namespace-bound
controller and owned-cluster validation harness.

The production control loop now uses the [ordinary hosting M1 contract](docs/features/kuberic/ordinary-hosting-m1.md):
format-16 semantic replica goals, early peer discovery, retained ordinary calls,
and exact Build outcomes before separate admission. Application state remains
provider-owned. Built-in/KV/SQLite and independent PostgreSQL use engine-owned
authenticated sessions and actual ordinary joining. This remains an experimental
development checkpoint; comprehensive recovery/failover qualification is M3.

The retained default Replicator composes source-guided Service Fabric classic V1
owners. Assignment, quorum completion and service-local commitment remain
separate; bounded integration is not complete M3 or live deployment qualification.

## Repository

```text
kuberic-runtime/           Application/replicator API, host, protocol, control
kuberic-controller/        CRDs, reconciliation policy and Kubernetes controller
kuberic-level-tests/       Unpublished in-process application and owned KinD journeys
kuberic-public-v1-tests/   Unpublished helper-free public provider/contract fixtures

examples/kvstore2/         Level-triggered conformance application
examples/sqlite/           SQLite Public V1 application
examples/postgres/         PostgreSQL-native Public V1 custom replicator
```

There are exactly two public production crates: `kuberic-runtime` and
`kuberic-controller`. The examples and level tests remain separate,
unpublished workspace packages (`publish = false`).
Applications depend only on `kuberic-runtime`; its default `host` feature supplies
`host::ReplicaHost`. Shared contracts are under `protocol` and `control`.
The runtime's `testing` feature provides isolated fixtures without exporting
production capabilities. The controller's opt-in `test-util` feature exposes
the [downstream in-process harness](docs/features/kuberic/in-process-testing.md)
for application dev-dependencies. Checkout-only conformance helpers remain
separately gated. The SQLite example owns its application-specific WAL
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
cargo nextest list --workspace --all-features --profile ordinary
cargo nextest run --workspace --all-features --profile ordinary
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
just nextest-source
```

The complete tier inventory and shard commands are documented in the
[test strategy](docs/features/kuberic/testing.md).

The archive contains all nextest-discoverable test binaries. Source/API/package
checks and doctests run in a separate checkout-backed CI job; ordinary
application/provider tests execute from archive partitions.

## Experimental Kubernetes Validation

The retained controller and KVStore2 Dockerfiles/owned-cluster recipes are
development assets, not published release images. Their old default-engine
bootstrap/replacement/failover scenarios are **not supported or newly qualified
at M1**. Wait for M2 engine integration before treating them as deployment
instructions. Generic M1 acceptance uses real agent transport with an in-process
Kubernetes storage boundary; it does not claim live-cluster validation.

## Applications

### KVStore2

The retained pre-M1 conformance application exercised the level-triggered controller,
agent and runtime path through an HTTP key/value API. Its retained isolated
live scenarios cover fresh bootstrap with process-session renewal,
same-cardinality replacement with preserved data, and sustained-outage
failover with old-primary fencing; the broader behavioral matrix runs in the
cluster-free runtime/provider tiers. Ordinary production Open now rejects
explicitly until M2; these historical journeys are not current formation proof.

### SQLite

`sqlite-replicated` uses quorum-before-publication WAL-frame replication,
committed snapshots and retained catch-up through Public V1. Fresh replacement
storage is required; prior provider state is rejected unchanged. See the
[SQLite design](docs/features/sqlite/design.md).
Ordinary production integration is unavailable until M2.

### PostgreSQL

`postgres-replicated` is an application-owned custom replicator.
PostgreSQL owns WAL streaming, physical recovery, synchronous policy and
promotion while Kuberic owns generic authority and lifecycle choreography.
It uses the Public V1 lifecycle and owns rejection of its prior provider
formats. See the
[PostgreSQL design](docs/features/postgres/design.md).
Ordinary production integration is unavailable until M2; native component
qualification does not imply replacement-session support.

## Distribution Status

Retired Kuberic classic application/operator source and image publication have
been removed. Existing external classic registry artifacts, if still present,
are unsupported historical artifacts.

V2 distribution is deferred. Main-branch and version-tag workflows currently
publish no Kuberic controller or application images. Build from source and use
the isolated development/CI assets above.

## Documentation

- [Level-triggered operator](docs/features/kuberic/level-triggered-operator.md)
- [Ordinary hosting M1 — implemented contract and checkpoint limits](docs/features/kuberic/ordinary-hosting-m1.md)
- [Classic V1 component semantics and pre-M1 evidence](docs/features/kuberic/classic-v1-runtime.md)
- [SF hosting target — M1 delivered, M2/M3 proposed](docs/features/kuberic/engine-neutral-hosting.md#proposed--ordinary-engine-boundary)
- [Future runtime roadmap and remaining qualification](docs/features/kuberic/sf-v1-semantic-alignment-plan.md)
- [SF V1 current evidence/dispositions](docs/features/kuberic/sf-v1-port-ledger.md) and [source provenance/notices](docs/features/kuberic/sf-v1-port-provenance.md)
- [V2 transactional Replicator design](docs/features/kuberic/v2-transactional-replicator-design.md)
- [Replicator ownership boundary](docs/features/kuberic/replicator-boundary.md)
- [Testing strategy](docs/features/kuberic/testing.md)
- [Downstream in-process application testing](docs/features/kuberic/in-process-testing.md)
- [Downstream in-process testing harness design](docs/proposal/downstream-in-process-testing.md)
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
