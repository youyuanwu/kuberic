# Copilot Instructions — Kubelicate (xedio)

## What This Is

A Service Fabric-inspired stateful replication system for Kubernetes.
Provides quorum-based replication with automatic failover, switchover,
copy-based replica building, and epoch-based fencing.

## Build & Test

```bash
cargo check                                    # fast type check
cargo clippy --all-targets                     # lint (must be zero warnings)
cargo test -p kubelicate-core -p kvstore -p sqlite-replicated   # run all meaningful tests
cargo fmt                                      # format before committing
```

- The `kubelicate-tests` crate requires a K8s cluster — skip it in local dev.
- All kvstore and sqlite tests use `#[serial]` (port contention).
- The build buffer replay test (`operator_replication.rs`) takes ~7s (500+200 entries).

## Architecture

```
kubelicate-operator/     K8s operator (reconciler, CRD, ClusterApi)
kubelicate-core/         Core replication framework
  driver.rs              PartitionDriver — SF RA equivalent, orchestrates lifecycle
  pod.rs                 PodRuntime — per-pod runtime, hosts replicator + user service
  replicator/
    actor.rs             WalReplicatorActor — single select loop, non-blocking
    primary.rs           PrimarySender — unbounded per-secondary channels
    secondary.rs         SecondaryReceiver — gRPC data plane
    queue.rs             ReplicationQueue — BTreeMap<Lsn, Bytes>, copy LSN boundary
    quorum.rs            QuorumTracker — write quorum + must_catch_up
  grpc/
    handle.rs            GrpcReplicaHandle — remote ReplicaHandle via gRPC
    server.rs            ControlServer — gRPC control plane
  handles.rs             PartitionState, StateReplicatorHandle, PartitionHandle
  types.rs               Epoch, Role, Lsn, ReplicaInfo, etc.
examples/kvstore/    Example KV store app using the framework
  src/testing.rs         Shared test utilities (KvPod, connect_kv_client)
  tests/                 Integration tests (operator_basic, failover, replication, reconciler)
examples/sqlite/     Example replicated SQLite DB using the framework
  src/testing.rs         Test utilities (SqlitePod, connect_sqlite_client)
  tests/                 Integration tests (operator_basic, failover, replication)
```

## Key Design Patterns

- **SF Phase 4 epoch distribution:** Epoch is sent to secondaries AFTER
  promotion (not before). Unreachable secondaries are skipped (best-effort).
- **Write revocation (A2):** `revoke_write_status()` RPC on old primary
  before switchover demotion. Matches SF SwapPrimary Phase 0.
- **Switchover rollback (A3):** If target promotion fails, re-promote old
  primary. Matches SF AbortPhase0Demote + RevertConfiguration.
- **ReplicationQueue:** In-memory BTreeMap retains ops for replay to new
  replicas. Copy LSN boundary (`PartitionState.copy_lsn_map`) ensures
  precise replay — no overlap with copy stream (C0 fix).
- **Non-blocking send_to_all:** Unbounded per-secondary channels with
  background drain tasks. Actor never blocks on slow secondaries.
- **Copy stream completion:** KV service waits for copy drain to finish
  naturally on IdleSecondary → ActiveSecondary (no cancellation).

## Design Documents

- `docs/features/kubelicate/` — main design (protocols, operator, user-api, status)
- `docs/features/kubelicate/design-gaps.md` — tracked gaps with SF source analysis
- `docs/features/kubelicate/testing.md` — test strategy and layers
- `docs/background/service-fabric/` — SF architecture reference (split into architecture, replication, failover, state-management, references)
- `docs/background/cloudnative-pg-architecture.md` — CNPG comparison
- `docs/features/sqlite/` — replicated SQLite design (WAL frame shipping, persist-then-ACK)
- `docs/features/operator-failure-scenarios.md` — failure handling designs
- `build/service-fabric/` — SF C++ source (depth-1 clone, for reference)

## Conventions

- Use `tracing` (info/warn/debug) — per-op traces at `debug!`, lifecycle at `info!`
- `ReplicaHandle` trait abstracts gRPC vs in-process (testing) handles
- Tests use real gRPC (no mocks) via `KvPod` helper and `GrpcReplicaHandle`
- `#[serial]` on all KV tests, `testing` feature flag for shared test utilities
- Design gaps tracked in `design-gaps.md` with SF source citations
- **Do not git commit unless explicitly instructed** — stage changes but wait for user
