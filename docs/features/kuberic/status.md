# Kuberic: Status and Roadmap

Implementation status, known gaps, and open questions.

> Part of the [Kuberic Design](../kuberic-replicator-design.md).

---

## MVP Simplifications

| Full Design | Current Status |
|---|---|
| Replicator WAL on disk | No WAL — replicator queues in memory, user `acknowledge()` releases |
| Incremental catchup via WAL replay | Full rebuild via copy stream (no WAL to replay) |
| build_replica copies state + replays WAL | Full state copy via data-plane CopyStream RPC |
| Copy stream delivers full state | ✅ Implemented — GetCopyContext + GetCopyState + CopyStream on data plane |
| `acknowledge()` gates quorum (persisted mode) | ✅ Implemented — SecondaryReceiver defers ACK until user acknowledges |
| `must_catch_up` enforced in catchup | ✅ Implemented — QuorumTracker tracks per-replica ACK progress |
| Build completes on copy+repl ACKed | ✅ Implemented — PrimarySender buffers ops during copy, replays on connect |
| mTLS on all gRPC | Deferred — assumes trusted cluster |
| ReportFault rate limiting | Operator-side, basic |
| Failover delay | ❌ Not implemented — optional K8s adaptation, default 0 (SF-aligned) |
| Data loss failover | ❌ Not implemented — SF `on_data_loss()` protocol for quorum loss |
| Secondary health detection | ❌ Not implemented — designed, see operator-failure-scenarios.md §2 |
| Missing pod detection | ❌ Not implemented — designed, see operator-failure-scenarios.md §3 |
| gRPC failure tracking | ❌ Not implemented — K8s adaptation of SF federation heartbeats |
| Operator restart recovery | ❌ Not implemented — designed, PartitionDriver::recover() |
| Primary self-fencing (liveness probe) | ❌ Not implemented — K8s defense-in-depth (from CNPG) |
| Node drain handling | ❌ Not implemented — K8s adaptation (analogous to SF PLB) |
| CRD conditions (Ready, Degraded, Quorum) | ❌ Not implemented — K8s addition |
| force_remove_secondary | ❌ Not implemented — SF `RemoveFromCurrentConfiguration` |

---

## Known Gaps vs SF

1. **WAL persistence.** The replicator has no write-ahead log. State survives
   only in the user's state provider. This means incremental catchup (replay
   from WAL) is not possible — every restart requires a full copy rebuild.

2. **mTLS.** All gRPC channels are unencrypted. Production deployments need
   mTLS or equivalent pod-to-pod encryption.

---

## Known Gaps vs CNPG (deliberate divergences)

CNPG patterns we **adopted** (K8s-specific value-adds):
1. Pod anti-affinity generation from CRD spec
2. Primary self-fencing via liveness probe isolation check
3. gRPC failure tracking (replaces CNPG's HTTP failure tracking)
4. Node drain detection with proactive switchover
5. CRD conditions (Ready, Degraded, QuorumAvailable)
6. Operator restart recovery from CRD status + pod list
7. Optional failover delay for environments with flappy probes

CNPG patterns we **rejected** (conflict with SF model):
1. **`R + W > N` quorum formula.** CNPG uses Dynamo-style quorum for PostgreSQL
   sync standbys. We use SF's `WriteQuorum = ⌊N/2⌋+1` which is simpler and
   already handled by `QuorumTracker`.
2. **Blocking on quorum loss.** CNPG blocks failover indefinitely when quorum
   check fails. SF waits `QuorumLossWaitDuration` then proceeds via the
   `on_data_loss()` callback. Our model has the user's state provider as
   the data loss decision point.
3. **Annotation-based fencing.** CNPG fences instances via a JSON annotation.
   We use SF's epoch-based fencing (replicator-level, stronger).
4. **pg_rewind for replica rejoin.** CNPG rewinds the old primary's WAL to
   rejoin as replica. We use full copy rebuild (no WAL on disk).

---

## Degenerate Configurations

**replicas=1:** No replication, no failover. Pod restart = resume as primary.
Development/testing only.

**replicas=2:** Write quorum=2 (both must ACK). Zero write fault tolerance.
Single failure → NoWriteQuorum. Failover is safe (survivor has all data).

**Minimum recommended:** `replicas >= 3`. Pod anti-affinity across nodes
(ensures failure independence for quorum model).

---

## Open Questions

1. **Multi-partition support** — multiple independent replica sets per
   KubericSet. SF supports this via partitioning schemes. Current
   design: one partition per CRD.
2. **Pod identity and PVC binding** — how to bind recreated pods to correct
   PVCs. Relevant for WAL persistence.
3. **Operator state recovery** — reconstructing PartitionDriver from CRD
   status after operator restart. Design in `operator-failure-scenarios.md` §8.
   Needs `PartitionDriver::recover()` method. SF's FM is similarly stateless,
   reconstructing from the FailoverUnit persistent state.
4. **force_remove_secondary** — removing a dead replica from quorum when
   gRPC calls to it fail. SF has `RemoveFromCurrentConfiguration` (config-first
   removal) and `RemoveReplica` (cancel in-progress builds). We need both
   error-tolerant paths.
5. **Data loss protocol integration** — the `on_data_loss()` callback exists
   in our user API but the operator never triggers it. Need to implement
   quorum loss detection and `data_loss_number` increment in the driver.
6. **Liveness probe HTTP endpoint** — PodRuntime currently exposes only gRPC.
   The self-fencing liveness probe (K8s-specific addition) needs an HTTP
   health endpoint. This is not an SF pattern — it compensates for K8s
   lacking SF's federation-level failure detection.

---

## Source Code Map

```
kuberic-core/
├── proto/kuberic.proto           # gRPC: ReplicatorControl (11 RPCs) + ReplicatorData (3 RPCs)
├── src/
│   ├── types.rs                     # Epoch, Role, AccessStatus, ReplicaInfo, Operation, OperationStream
│   ├── error.rs                     # KubericError enum (NotPrimary, NoWriteQuorum, etc.)
│   ├── events.rs                    # LifecycleEvent, StateProviderEvent, ReplicatorControlEvent
│   ├── handles.rs                   # PartitionState (atomics), PartitionHandle, StateReplicatorHandle
│   ├── noop.rs                      # NoopReplicator actor (testing)
│   ├── runtime.rs                   # KubericRuntime (lower-level harness)
│   ├── pod.rs                       # PodRuntime (full pod: actor + gRPC + user events + copy protocol)
│   ├── driver.rs                    # PartitionDriver + ReplicaHandle trait + InProcessReplicaHandle
│   ├── replicator/
│   │   ├── quorum.rs                # QuorumTracker (single + dual-config + must_catch_up + baseline)
│   │   ├── queue.rs                 # ReplicationQueue (in-memory op retention for replay to new replicas)
│   │   ├── actor.rs                 # WalReplicatorActor (control + data event loop + ReplicationQueue)
│   │   ├── primary.rs               # PrimarySender (unbounded per-secondary channels + replay)
│   │   └── secondary.rs             # SecondaryReceiver (replication + copy gRPC server)
│   └── grpc/
│       ├── convert.rs               # Proto ↔ domain type conversions
│       ├── server.rs                # ControlServer (routes through PodRuntime command loop)
│       └── handle.rs                # GrpcReplicaHandle (remote ReplicaHandle for operator)

kuberic-operator/
├── src/
│   ├── lib.rs                       # Public API (cluster_api, crd, reconciler)
│   ├── main.rs                      # Binary entry point (kube controller)
│   ├── crd.rs                       # KubericSet CRD with spec/status/enums
│   ├── cluster_api.rs               # ClusterApi trait + KubeClusterApi impl
│   ├── reconciler.rs                # Reconcile loop (Pending→Creating→Healthy→FailingOver→Switchover)
│   └── tests.rs                     # Mock reconciler tests

examples/kvstore/
├── src/
│   ├── lib.rs                       # Module declarations
│   ├── main.rs                      # Binary entry point
│   ├── state.rs                     # KvOp, KvState, drain_stream
│   ├── server.rs                    # Client-facing KV gRPC server
│   ├── service.rs                   # Lifecycle + StateProvider event loop
│   ├── demo.rs                      # Operator/client simulators
│   ├── tests.rs                     # Operator-driven integration tests (6)
│   └── reconciler_tests.rs          # Reconciler-driven E2E tests (2)
└── proto/kvstore.proto              # Client KV API (Get/Put/Delete)
```

**Tests:** See [Testing](testing.md) for full test strategy, layers, and listing.

---

## Documentation Index

- `docs/background/service-fabric/README.md` — SF architecture study
- `docs/background/cloudnative-pg-architecture.md` — CNPG architecture study
- `docs/features/kuberic-replicator-design.md` — Main design (this doc's parent)
- `docs/features/kuberic/protocols.md` — Replication protocols
- `docs/features/kuberic/operator.md` — Operator design
- `docs/features/kuberic/user-api.md` — User application model
- `docs/features/kuberic/status.md` — Status and roadmap (this doc)
- `docs/features/kvstore-design.md` — KV store example design
- `docs/features/operator-failure-scenarios.md` — Failure scenarios + recovery
