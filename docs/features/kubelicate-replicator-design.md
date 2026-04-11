# Kubelicate: SF-Style Stateful Replication on Kubernetes

High-level design for a Service Fabric–inspired stateful replication system
running on Kubernetes. The operator acts as the Failover Manager; user
application pods run an in-process replicator that reacts to operator signals.

**Implementation:** `kubelicate-core`, `kubelicate-operator`, `kvstore` example.
See [Status & Roadmap](kubelicate/status.md) for LOC counts and
[Testing](kubelicate/testing.md) for test coverage.

---

## Documents

| Document | Contents |
|----------|----------|
| **This file** | Architecture overview, key design decisions |
| [Protocols](kubelicate/protocols.md) | Failover, switchover, create, scale-up/down, restart, access status, epoch fencing, quorum model |
| [Operator](kubelicate/operator.md) | CRD spec/status, reconciler phases, healthy phase, gRPC tracking, scale-up/down, restart recovery |
| [User API](kubelicate/user-api.md) | Lifecycle events, state provider callbacks, dual-channel model, operation streams, copy protocol |
| [Status & Roadmap](kubelicate/status.md) | MVP simplifications, known gaps (SF + CNPG), degenerate configs, open questions, source code map |
| [Testing](kubelicate/testing.md) | Test layers (4), infrastructure, what's tested vs not, how to run |
| [Design Gaps](kubelicate/design-gaps.md) | 27 identified gaps: protocol safety (5), operational resilience (5), correctness (3), implementation-only (14) |
| [Failure Scenarios](operator-failure-scenarios.md) | 9 failure scenarios with detection + recovery design |
| [KV Store Example](kvstore-design.md) | KV store example design and test listing |

### Background References

| Document | Contents |
|----------|----------|
| [SF Architecture](../background/service-fabric/README.md) | SF replicator, failover, switchover, epochs, quorum, data loss, Rust API |
| [CNPG Architecture](../background/cloudnative-pg-architecture.md) | CNPG operator, failover, fencing, failure scenarios, instance manager |

---

## Architecture Overview

```
┌──────────────────────────────────────────────────────────────────┐
│                      Kubernetes Cluster                          │
│                                                                  │
│  ┌──────────────────────┐                                        │
│  │ Kubelicate Operator  │   Watches: KubelicateSet CRD           │
│  │ (future crate)       │   Uses: PartitionDriver + GrpcReplica- │
│  │                      │   Handle to drive remote pods           │
│  └──────────┬───────────┘                                        │
│             │ gRPC (ReplicatorControl)                           │
│             ▼                                                    │
│  ┌───────────────────────────────────────────────────────┐       │
│  │               Replica Set (3 Pods)                    │       │
│  │                                                       │       │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐   │       │
│  │  │ Pod 1       │  │ Pod 2       │  │ Pod 3       │   │       │
│  │  │ PRIMARY     │  │ ACTIVE SEC  │  │ ACTIVE SEC  │   │       │
│  │  │             │  │             │  │             │   │       │
│  │  │ PodRuntime  │  │ PodRuntime  │  │ PodRuntime  │   │       │
│  │  │ ┌─────────┐ │  │ ┌─────────┐ │  │ ┌─────────┐ │   │       │
│  │  │ │Replicatr│◄├──├─┤Replicatr│ ├──├─┤Replicatr│ │   │       │
│  │  │ │Actor    │ │  │ │Actor    │ │  │ │Actor    │ │   │       │
│  │  │ └────┬────┘ │  │ └─────────┘ │  │ └─────────┘ │   │       │
│  │  │      │      │  │             │  │             │   │       │
│  │  │ User App    │  │ User App    │  │ User App    │   │       │
│  │  │ (events)    │  │ (standby)   │  │ (standby)   │   │       │
│  │  └─────────────┘  └─────────────┘  └─────────────┘   │       │
│  └───────────────────────────────────────────────────────┘       │
│                                                                  │
│  Services:                                                       │
│    {name}-rw  ──► Primary       (label: role=primary)            │
│    {name}-ro  ──► Secondaries   (label: role=secondary)          │
└──────────────────────────────────────────────────────────────────┘
```

---

## Key Design Decisions

| Decision | Choice | Rationale |
|---|---|---|
| **No StatefulSets** | Operator manages Pods directly | LSN-based primary selection, per-instance control |
| **Replicator is in-process** | Library linked into app, not sidecar | No coordination overhead, shared memory |
| **Operator-managed membership** | Operator pushes ReplicaSetConfig | Dual-config quorum, must-catch-up markers, idle/active gating |
| **Event-based APIs** | Both replicator and user use mpsc channels | Owned `&mut` state, no `Arc<Mutex<...>>` |
| **Dual-channel replicator** | Separate control (low-freq) and data (high-freq) channels | Control events don't block write path |
| **Atomic status reads** | PartitionState uses AtomicU8/AtomicI64 | Zero-cost read_status()/write_status() polling |
| **Fence-before-promote** | update_epoch to all secondaries BEFORE change_role(Primary) | Prevents zombie primary writes to unfenced nodes |
| **State provider as durability layer** | Persisted mode: `acknowledge()` gates quorum (SF default) | Strongest guarantee: replicate() returns only after quorum applied |
| **Primary self-fencing** | Liveness probe isolation check (CNPG-style) | Defense-in-depth for asymmetric partitions |
| **Failover delay** | Optional `spec.failoverDelay` (default 0 = immediate) | K8s adaptation — SF failovers immediately, K8s pod probes can flap |
| **Data loss protocol** | Failover always proceeds; `on_data_loss()` if quorum lost | SF FM pattern — system always makes progress, user decides |
| **gRPC failure tracking** | Per-replica failure counter in CRD status | K8s adaptation — replaces SF federation heartbeats |
| **Operator restart recovery** | Reconstruct driver from CRD status + pod list | SF FM pattern — stateless operator, durable state in API |
| **mTLS deferred** | Post-MVP; MVP assumes trusted cluster | Reduces initial complexity |
| **ReplicaHandle trait** | Driver works with any transport | Tests: in-process channels. Operator: gRPC client. |

