# Kubelicate: SF-Style Stateful Replication on Kubernetes

High-level design for a Service Fabric–inspired stateful replication system
running on Kubernetes. The operator acts as the Failover Manager; user
application pods run an in-process replicator that reacts to operator signals.

This document focuses on the **replicator interface and failover/fencing
protocol** — the contract between operator and app. State manager internals,
copy streams, and WAL storage are out of scope.

---

## Table of Contents

1. [Goals and Non-Goals](#goals-and-non-goals)
2. [Architecture Overview](#architecture-overview)
3. [Mapping SF Concepts to Kubernetes](#mapping-sf-concepts-to-kubernetes)
4. [Operator Design (Failover Manager)](#operator-design-failover-manager)
5. [Replicator Interface](#replicator-interface)
6. [Error Types](#error-types)
7. [Partition Access Status (ReadStatus / WriteStatus)](#partition-access-status)
8. [Epoch and Fencing Protocol](#epoch-and-fencing-protocol)
9. [Operator Workflow Sequences](#operator-workflow-sequences)
10. [Failover Sequence](#failover-sequence)
11. [Switchover Sequence](#switchover-sequence)
12. [Reconfiguration Phases](#reconfiguration-phases)
13. [Replication Data Path](#replication-data-path)
14. [Quorum Calculation](#quorum-calculation)
15. [Data Loss Detection](#data-loss-detection)
16. [Operator Crash Recovery](#operator-crash-recovery)
17. [Concurrent Reconfiguration Safety](#concurrent-reconfiguration-safety)
18. [Operator ↔ Pod Communication](#operator--pod-communication)
19. [User Application Programming Model](#user-application-programming-model)
20. [Degenerate Configurations](#degenerate-configurations)
21. [Build Replica Idempotency](#build-replica-idempotency)
22. [Reconfiguration Abort Behavior](#reconfiguration-abort-behavior)
23. [Access Status: Phase3_Deactivate](#access-status-phase3_deactivate)
24. [Epoch Type Migration](#epoch-type-migration)
25. [Open Questions](#open-questions)

---

## Goals and Non-Goals

### Goals

- **SF-like replicator semantics on Kubernetes** — quorum-based replication
  with epoch fencing, LSN-based primary election, and staged access status
  transitions during reconfiguration.
- **Operator as Failover Manager** — a Kubernetes operator (CRD + controller)
  that detects failures, selects new primaries, and orchestrates
  reconfigurations by signaling pods.
- **Clear replicator trait contract** — a Rust trait interface that user
  applications implement, analogous to SF's `IReplicator` /
  `IPrimaryReplicator`. The `kubelicate-core` crate contains the types and
  trait definitions. (The older `kubelicate-data` crate is reference code only.)
- **ReadStatus / WriteStatus fencing** — user code can query whether reads and
  writes are currently safe, with the same staged-revocation semantics as SF.
- **Graceful switchover** — planned primary changes with double-catchup and
  staged write/read revocation, minimizing client disruption.

### Non-Goals (for this design)

- State manager / Reliable Collections implementation details
- Copy stream and WAL storage internals (reference code in `kubelicate-data`)
- Service discovery and client routing
- Multi-partition support (start with singleton partitions)
- Backup and restore

### MVP Simplifications

The initial implementation can cut scope in the replication data path:

| Full Design | MVP | Impact |
|---|---|---|
| Replicator persists to its own WAL on disk, then feeds state provider | **No replicator WAL.** State provider is the durability layer. | No replicator WAL layer to build. |
| Catchup via WAL replay (send missing LSN range from replicator log) | **Catchup = full rebuild** via copy stream from state provider. | Slower secondary restarts, but correct. |
| `build_replica` copies state up to LSN X, then replays WAL from X | **`build_replica` does full state copy only.** | Simpler, no WAL/copy coordination. |
| Replicator crash recovery from its own WAL | **Rely on state provider durability.** Pod restart = rejoin as idle + full rebuild. | State provider must be durable. |

**MVP durability contract:** The replicator does NOT persist operations
itself. Instead, **secondaries must apply operations to the state provider
and persist them BEFORE sending an ACK** to the primary. This makes the state
provider the durability layer. A quorum of ACKs means a quorum of replicas
have durably persisted the operation. If a secondary ACKs but crashes before
persisting, the quorum guarantee is violated — so the ACK must come after
the state provider's synchronous persist.

**What the MVP preserves** (all the failover/fencing design in this doc):
- Quorum replication (ACK after state provider persist)
- Epoch-based fencing
- ReadStatus / WriteStatus lifecycle and staged revocation
- LSN-based primary election
- Failover and switchover sequences
- Operator-managed membership with dual-config quorum
- Event-based user API

The replicator WAL can be added later as an optimization without changing the
operator protocol or user API — it's an internal implementation detail of the
replicator.

---

## Architecture Overview

```
┌──────────────────────────────────────────────────────────────────┐
│                      Kubernetes Cluster                          │
│                                                                  │
│  ┌──────────────────────┐                                        │
│  │ Kubelicate Operator  │   Watches: KubelicateSet CRD           │
│  │ (Deployment)         │   Manages: Pods, ConfigMaps, Services  │
│  │                      │                                        │
│  │  ┌────────────────┐  │                                        │
│  │  │ Failover Mgr   │  │   Detects failures                    │
│  │  │ (per partition) │──┤   Selects primaries (by LSN)          │
│  │  │                │  │   Sends reconfigurations               │
│  │  └────────────────┘  │                                        │
│  └──────────┬───────────┘                                        │
│             │                                                    │
│             │  gRPC or CRD status                                │
│             │  (DoReconfiguration signals)                       │
│             ▼                                                    │
│  ┌───────────────────────────────────────────────────────┐       │
│  │               Replica Set (3 Pods)                    │       │
│  │                                                       │       │
│  │  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐   │       │
│  │  │ Pod 1       │  │ Pod 2       │  │ Pod 3       │   │       │
│  │  │ PRIMARY     │  │ ACTIVE SEC  │  │ ACTIVE SEC  │   │       │
│  │  │             │  │             │  │             │   │       │
│  │  │ ┌─────────┐ │  │ ┌─────────┐ │  │ ┌─────────┐ │   │       │
│  │  │ │Replicatr│ │  │ │Replicatr│ │  │ │Replicatr│ │   │       │
│  │  │ │(library)│ │  │ │(library)│ │  │ │(library)│ │   │       │
│  │  │ └────┬────┘ │  │ └────┬────┘ │  │ └────┬────┘ │   │       │
│  │  │      │      │  │      │      │  │      │      │   │       │
│  │  │ ┌────┴────┐ │  │ ┌────┴────┐ │  │ ┌────┴────┐ │   │       │
│  │  │ │User App │ │  │ │User App │ │  │ │User App │ │   │       │
│  │  │ │(RunAsync│ │  │ │(standby)│ │  │ │(standby)│ │   │       │
│  │  │ │+ writes)│ │  │ │         │ │  │ │         │ │   │       │
│  │  │ └─────────┘ │  │ └─────────┘ │  │ └─────────┘ │   │       │
│  │  └─────────────┘  └─────────────┘  └─────────────┘   │       │
│  └───────────────────────────────────────────────────────┘       │
│                                                                  │
│  Services:                                                       │
│    {name}-rw  ──► Primary Pod        (label: role=primary)       │
│    {name}-ro  ──► Secondary Pods     (label: role=secondary)     │
│    {name}-r   ──► All ready Pods                                 │
└──────────────────────────────────────────────────────────────────┘
```

Key design decisions (inherited from SF, adapted for k8s):

- **No StatefulSets** — operator manages Pods directly (like CNPG), enabling
  LSN-based primary selection rather than ordinal-based.
- **Replicator is an in-process library** — no sidecar. The replicator is a
  Rust library (`kubelicate-core`) linked into the user application. It
  manages quorum replication, epoch tracking, and access status.
- **Operator signals via CRD status + gRPC** — the operator writes target
  configuration into the CRD status; the replicator in each pod reads it and
  executes the reconfiguration phases.

---

## Mapping SF Concepts to Kubernetes

| SF Concept | Kubernetes Equivalent | Notes |
|---|---|---|
| Failover Manager (FM) | **Operator controller** (ClusterReconciler) | Detects pod failures, selects primary, initiates reconfiguration |
| Reconfiguration Agent (RA) | **Replicator library** (in-pod) | Executes reconfiguration phases locally, reports status back |
| Partition | **KubelicateSet CRD** | One CRD instance = one replica set (start with singleton) |
| Replica | **Pod** | Each pod runs one replica of the user application |
| Epoch | **CRD status field** | `epoch { data_loss_number, configuration_number }` |
| Configuration (PC/CC) | **CRD status fields** | `previous_configuration`, `current_configuration` |
| ReplicaRole | **Pod label** `kubelicate.io/role` | `primary`, `active-secondary`, `idle-secondary`, `none` |
| Service endpoints | **Kubernetes Services** | `-rw`, `-ro`, `-r` services with label selectors |
| ReadStatus/WriteStatus | **In-process state** | Computed by replicator library, exposed to user code |
| ReportFault | **Pod annotation or gRPC** | App signals replicator → replicator signals operator |
| DoReconfiguration | **CRD status update** | Operator writes target config; pod replicator watches it |

---

## Operator Design (Failover Manager)

### CRD: KubelicateSet

```yaml
apiVersion: kubelicate.io/v1
kind: KubelicateSet
metadata:
  name: my-app
spec:
  replicas: 3
  minReplicas: 2                           # Write quorum = minReplicas
  image: my-app:latest
  failoverDelay: 0                         # Seconds before triggering failover
  switchoverDelay: 3600                    # Max seconds for graceful demotion

status:
  epoch:
    dataLossNumber: 0
    configurationNumber: 5

  currentPrimary: my-app-2                 # Pod currently serving as primary
  targetPrimary: my-app-2                  # Desired primary ("pending" during failover)

  phase: Healthy                           # Healthy | FailingOver | Switchover | ...

  currentConfiguration:
    writeQuorum: 2
    members:
      - id: my-app-1
        role: ActiveSecondary
        currentProgress: 142               # LSN
        catchUpCapability: 1
        status: Up
      - id: my-app-2
        role: Primary
        currentProgress: 142
        catchUpCapability: 1
        status: Up
      - id: my-app-3
        role: ActiveSecondary
        currentProgress: 140
        catchUpCapability: 1
        status: Up

  previousConfiguration: null              # Non-null during reconfiguration

  reconfigurationPhase: None               # None | Phase1_GetLSN | Phase2_Catchup
                                           # | Phase3_Deactivate | Phase4_Activate
                                           # | Phase0_Demote (swap only)
```

### Operator Reconciliation Loop

On each reconciliation:

1. **Poll pod replicator status** — gRPC `GetStatus` on each pod reports:
   role, LSN (current progress + catch-up capability + committed), epoch, health
2. **Detect failures** — pod not reporting (gRPC timeout), pod reporting errors, node down
3. **Evaluate failover conditions:**
   - Is `targetPrimary` healthy? If not, start failover
   - Is primary on an unschedulable node? If so, trigger switchover
   - Has `failoverDelay` elapsed since primary started failing?
4. **Select new primary** — sort healthy replicas by:
   a. Current progress LSN (descending — most data wins)
   b. Catch-up capability LSN (ascending — can serve oldest data)
   c. Pod name (alphabetical tiebreaker)
5. **Write reconfiguration** — update CRD status with new target primary,
   new epoch, new configuration, and reconfiguration phase
6. **Monitor phases** — on subsequent reconciliations, advance through phases
   as pods report progress
7. **Update labels and services** — once reconfiguration completes, update pod
   labels to route traffic correctly

### Operator Controller Structure

The operator is a standard `kube-runtime` controller written in Rust:

```rust
// Crate: kubelicate-operator (separate binary, not in kubelicate-core)

struct KubelicateSetReconciler {
    client: kube::Client,
    // gRPC client pool — one connection per known pod
    replicator_clients: HashMap<String, ReplicatorControlClient>,
}

impl KubelicateSetReconciler {
    async fn reconcile(
        &self,
        set: Arc<KubelicateSet>,  // The CRD object
        ctx: Arc<Context>,
    ) -> Result<Action> {
        // 1. List pods owned by this KubelicateSet
        let pods = self.list_managed_pods(&set).await?;

        // 2. Poll status from each pod via gRPC GetStatus
        let statuses = self.poll_all_statuses(&pods).await;

        // 3. Check current reconfiguration phase
        match set.status.reconfiguration_phase {
            Phase::None => {
                // Normal operation: check for failures, scale up/down
                self.reconcile_normal(&set, &pods, &statuses).await
            }
            phase => {
                // Resume in-progress reconfiguration
                self.resume_reconfiguration(&set, &pods, &statuses, phase).await
            }
        }
    }

    async fn reconcile_normal(&self, ...) -> Result<Action> {
        // Check if primary is healthy
        // Check if scale up/down needed
        // Check if switchover requested
        // If all healthy: requeue after 10s
    }

    async fn resume_reconfiguration(&self, ...) -> Result<Action> {
        // Drive the phase state machine:
        // Phase1_GetLSN → Phase2_Catchup → Phase3_Deactivate → Phase4_Activate
        // Each phase: check if done → advance to next → requeue
    }

    // gRPC helpers that wrap the proto client calls
    async fn fence_secondaries(&self, pods: &[Pod], epoch: Epoch) -> Result<()>;
    async fn promote_primary(&self, pod: &str, epoch: Epoch) -> Result<()>;
    async fn poll_status(&self, pod: &str) -> Result<GetStatusResponse>;
}

// Controller wiring (in main.rs of kubelicate-operator)
Controller::new(kubelicate_sets, watcher::Config::default())
    .owns(pods, watcher::Config::default())
    .run(reconcile, error_policy, Arc::new(ctx))
    .for_each(|res| async move { ... })
    .await;
```

**Key design rules for the operator:**
- **CRD status is the durable state machine.** Every phase transition is
  written to CRD status BEFORE gRPC calls are made (write-ahead).
- **All gRPC calls have timeouts** (default 30s). Timeout = pod unresponsive.
- **Reconciliation is idempotent.** The operator can crash and restart at any
  point — it reads CRD status and resumes the current phase.
- **One reconfiguration at a time.** If a new failure occurs during
  reconfiguration, the operator either incorporates it or aborts and restarts.

---

## Replicator Interface

The system has two distinct trait surfaces: a **system-internal** `Replicator`
trait (called by the operator) and a **user-facing** set of traits
(`ServiceEvent` channel, `Partition`, `StateReplicator`). User code never
implements or calls the `Replicator` trait — they select a replicator
implementation and implement their service logic against the user-facing
traits.

### Design Decision: Operator-Managed Membership (SF Model)

The operator **explicitly pushes replica set configuration** to the primary
replicator via `update_catch_up_replica_set_configuration` /
`update_current_replica_set_configuration`. The replicator does NOT discover
peers on its own from Kubernetes — it only knows about replicas the operator
told it about.

**Why not CNPG's approach?** CNPG relies on PostgreSQL's native streaming
replication where replicas self-register by connecting to the primary. Our
replicator is a library, not a database — it doesn't have built-in connection
management. It needs to be told explicitly who to replicate to.

**Why not peer-discovery from CRD status or DNS?** While simpler, it makes
dual-configuration quorum during reconfigurations very hard to implement
correctly. The SF model's explicit config push gives the operator precise
control over:

- **Write quorum calculation** — exactly which replicas count, and how many
  ACKs are needed
- **Dual-config quorum** — during reconfiguration, writes must be
  acknowledged by quorum from BOTH previous and current configurations. This
  requires the replicator to know both configs simultaneously.
- **must_catch_up markers** — exactly one replica (the primary candidate) is
  marked as must-catch-up during failover. Peer discovery can't express this.
- **Idle vs Active distinction** — a newly-built replica is idle (not in
  quorum) until explicitly promoted to active via config update. If the
  replicator auto-discovered it, it might incorrectly count it toward quorum
  before it has all the data.
- **Incremental membership changes** — secondaries are added to the
  configuration one at a time, each going through the full
  build → catch-up → promote cycle. This prevents a burst of new replicas
  from destabilizing quorum.

**CNPG comparison for scaling (3→4 replicas):**
CNPG creates a Job that runs `pg_basebackup`, then creates a Pod. The new
replica self-connects to the primary via streaming replication. The operator
only updates `synchronous_standby_names` for quorum policy. No explicit
config push to PostgreSQL.

**Our approach for scaling (3→4 replicas):**
1. Operator creates a new Pod
2. Pod opens its replicator in Idle role
3. Operator calls `build_replica(new_pod_info)` on primary's replicator
   (primary sends copy stream to idle replica)
4. Operator promotes idle → active via `replica.change_role(ActiveSecondary)`
5. Operator calls `update_catch_up_replica_set_configuration(CC with 4, PC with 3)`
6. Operator calls `wait_for_catch_up_quorum(Write)`
7. Operator calls `update_current_replica_set_configuration(CC with 4)`
8. New replica now counts toward quorum

### Design: Replica vs Replicator Separation

In SF, Replica (user code) and Replicator (system component) are separate COM
objects wired together at `open()` time. We preserve this separation because:

1. **Multiple replicator implementations** — different backends for different
   use cases (WAL-based, KV-based, test/no-op, custom storage engines)
2. **User code never sees replicator internals** — `build_replica`,
   `update_catch_up_config`, `wait_for_catch_up_quorum` are system concerns
3. **Clean testability** — a no-op replicator lets users test service logic
   without replication infrastructure

```
┌─────────────────────────────────────────────────────────────────┐
│ User-facing (user receives / calls)                             │
│                                                                 │
│  ServiceEvent channel     PartitionHandle      StateReplicator- │
│  (receive + process)      (atomic reads)       Handle           │
│  • Open                   • read_status()      • replicate()    │
│  • ChangeRole             • write_status()       (→ data_tx)    │
│  • Close                  • report_fault()                      │
│  • Abort                    (→ fault_tx)                        │
└─────────────────────────┬───────────────────────────────────────┘
                          │ wired at startup
┌─────────────────────────┴───────────────────────────────────────┐
│ System-internal: Replicator Actor (event-based, dual channel)   │
│                                                                 │
│  control_rx (mpsc)              data_rx (mpsc)                  │
│  ┌────────────────────┐         ┌────────────────────┐          │
│  │ • Open / Close     │         │ • ReplicateRequest │          │
│  │ • ChangeRole       │         │   (high-throughput  │          │
│  │ • UpdateEpoch      │  biased │    data path)       │          │
│  │ • BuildReplica     │◄────────│                    │          │
│  │ • UpdateConfig     │ priority│                    │          │
│  │ • WaitForCatchUp   │         └────────────────────┘          │
│  │ • OnDataLoss       │                                        │
│  │ • RemoveReplica    │         PartitionState (atomics)        │
│  └────────────────────┘         • read/write status             │
│                                 • current_progress              │
│  Implementations:               • committed_lsn                 │
│  • WalReplicator                                                │
│  • NoopReplicator                                               │
│  • (custom)                                                     │
└─────────────────────────────────────────────────────────────────┘
```

### Wiring at Pod Startup

```rust
// User configures which replicator to use at pod startup:
let (service_event_tx, service_event_rx) = mpsc::channel(16);

let runtime = KubelicateRuntime::builder()
    .replicator(WalReplicator::new(wal_config))     // select impl
    .service_event_sender(service_event_tx)          // user's event channel
    .build();

// Run user service event loop and runtime concurrently:
tokio::select! {
    result = runtime.run() => result?,
    _ = run_service(service_event_rx) => {},
}

// Internally, the runtime:
// 1. Creates replicator control + data channels
// 2. Spawns the replicator actor task (processes both channels)
// 3. Creates PartitionHandle + StateReplicatorHandle from shared PartitionState
// 4. Sends ServiceEvent::Open { ctx: ServiceContext { partition, replicator, token } }
// 5. Listens for operator gRPC commands
// 6. On operator gRPC call: sends ReplicatorControlEvent, awaits oneshot reply
// 7. On role change: sends ReplicatorControlEvent::ChangeRole to replicator FIRST,
//    THEN sends ServiceEvent::ChangeRole to user (promotion order)
//    or vice versa (demotion order)
// 8. User's replicate() calls go directly to data_rx — bypass the runtime
```

### Replicator: Event-Based with Dual Channels

Like the user API, the replicator is also **event-based**. The replicator
runs as an actor with two inbound channels, separating the control path
(low-frequency, from operator) from the data path (high-frequency, from
user writes):

```
                                 ┌──────────────────────────┐
  Operator (gRPC)                │     Replicator Actor     │
       │                         │                          │
       │── ChangeRole ──────────►│  control_rx (mpsc)       │
       │── UpdateEpoch ─────────►│  ┌──────────────────┐   │
       │── BuildReplica ────────►│  │ Process one at a  │   │
       │── UpdateConfig ────────►│  │ time, sequentially│   │
       │── WaitForCatchUp ──────►│  └──────────────────┘   │
       │                         │                          │
  User code                      │                          │
       │                         │                          │
       │── replicate(data) ─────►│  data_tx (channel)       │
       │── replicate(data) ─────►│  ┌──────────────────┐   │
       │── replicate(data) ─────►│  │ Batched, high-    │   │
       │                         │  │ throughput path    │   │
       │                         │  └──────────────────┘   │
       │                         │                          │
  Runtime (status polling)       │                          │
       │                         │                          │
       │── get_progress ────────►│  status (atomic reads)   │
       │── get_capability ──────►│  (no channel needed)     │
       │                         │                          │
       │                         └──────────────────────────┘
```

**Why two channels?**
- Control operations (`ChangeRole`, `UpdateEpoch`) mutate the replicator's
  core state (current role, epoch, member list). They are rare and must be
  serialized — you can't process `UpdateEpoch` concurrently with `ChangeRole`.
- Data operations (`replicate`) are the hot path. They should not be blocked
  by a slow `BuildReplica` sitting in the same queue.
- Status reads (`get_current_progress`, `get_catch_up_capability`) are
  lock-free atomic reads — no channel needed.

### Replicator Control Events

```rust
/// Control events delivered to the replicator by the runtime.
/// Processed sequentially on the control channel.
pub enum ReplicatorControlEvent {
    Open {
        mode: OpenMode,
        reply: oneshot::Sender<Result<()>>,
    },
    Close {
        reply: oneshot::Sender<Result<()>>,
    },
    Abort,

    ChangeRole {
        epoch: Epoch,
        role: Role,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateEpoch {
        epoch: Epoch,
        reply: oneshot::Sender<Result<()>>,
    },

    // Primary-only reconfiguration commands
    OnDataLoss {
        reply: oneshot::Sender<Result<DataLossAction>>,
    },
    UpdateCatchUpConfiguration {
        current: ReplicaSetConfig,
        previous: ReplicaSetConfig,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateCurrentConfiguration {
        current: ReplicaSetConfig,
        reply: oneshot::Sender<Result<()>>,
    },
    WaitForCatchUpQuorum {
        mode: ReplicaSetQuorumMode,
        reply: oneshot::Sender<Result<()>>,
    },
    BuildReplica {
        replica: ReplicaInfo,
        reply: oneshot::Sender<Result<()>>,
    },
    RemoveReplica {
        replica_id: ReplicaId,
        reply: oneshot::Sender<Result<()>>,
    },
}
```

### Replicator Data Path

The data path for `replicate()` uses a separate channel so it is never
blocked by control operations:

```rust
/// Request to replicate data to quorum.
pub struct ReplicateRequest {
    pub data: Bytes,
    pub reply: oneshot::Sender<Result<Lsn>>,
}
```

The `StateReplicator` handle (given to user code) wraps a sender to the
data channel:

```rust
/// User-facing write handle. Backed by the replicator's data channel.
pub struct StateReplicatorHandle {
    data_tx: mpsc::Sender<ReplicateRequest>,
    partition: Arc<PartitionState>,  // for access status checks
}

impl StateReplicatorHandle {
    pub async fn replicate(
        &self, data: Bytes, token: CancellationToken,
    ) -> Result<Lsn> {
        // Fast-path access status check (avoids channel round-trip)
        match self.partition.write_status() {
            AccessStatus::Granted => {}
            AccessStatus::NotPrimary => return Err(KubelicateError::NotPrimary),
            AccessStatus::NoWriteQuorum => return Err(KubelicateError::NoWriteQuorum),
            AccessStatus::ReconfigurationPending => {
                return Err(KubelicateError::ReconfigurationPending)
            }
        }

        let (reply_tx, reply_rx) = oneshot::channel();
        self.data_tx
            .send(ReplicateRequest { data, reply: reply_tx })
            .await
            .map_err(|_| KubelicateError::Closed)?;

        tokio::select! {
            result = reply_rx => result.map_err(|_| KubelicateError::Closed)?,
            _ = token.cancelled() => Err(KubelicateError::Cancelled),
        }
    }
}
```

### Status Reads (Lock-Free)

Progress and capability are read by the operator for primary selection.
They use **atomics** — no channel, no lock:

```rust
/// Shared partition state. Read by multiple consumers, written by the
/// replicator actor on the control channel.
pub struct PartitionState {
    read_status: AtomicU8,       // AccessStatus
    write_status: AtomicU8,      // AccessStatus
    current_progress: AtomicI64, // Lsn
    catch_up_capability: AtomicI64, // Lsn
    committed_lsn: AtomicI64,   // Lsn
    current_epoch: Mutex<Epoch>, // Rarely read, needs both fields
}
```

The runtime reads these directly when handling `GetStatus` gRPC calls from
the operator — no round-trip through the replicator's event loop.

### Partition Interface (exposed to user code)

The `Partition` handle reads from the same `PartitionState` atomics:

```rust
/// User-facing partition handle. Reads access status from shared atomics.
pub struct PartitionHandle {
    state: Arc<PartitionState>,
    fault_tx: mpsc::Sender<FaultType>,  // → runtime → operator
}

impl Partition for PartitionHandle {
    fn read_status(&self) -> AccessStatus {
        self.state.read_status.load(Ordering::Acquire).into()
    }

    fn write_status(&self) -> AccessStatus {
        self.state.write_status.load(Ordering::Acquire).into()
    }

    fn report_fault(&self, fault_type: FaultType) {
        let _ = self.fault_tx.try_send(fault_type);
    }
}
```

### Replicator Event Loop (Implementation Skeleton)

```rust
/// The replicator actor. Owns all mutable state.
async fn replicator_run(
    mut control_rx: mpsc::Receiver<ReplicatorControlEvent>,
    mut data_rx: mpsc::Receiver<ReplicateRequest>,
    state: Arc<PartitionState>,
) {
    let mut role = Role::None;
    let mut epoch = Epoch::default();
    let mut members: Vec<ReplicaInfo> = vec![];
    let mut next_lsn: Lsn = 1;
    // ... other owned mutable state

    loop {
        tokio::select! {
            // Control events take priority (biased)
            biased;

            Some(event) = control_rx.recv() => {
                match event {
                    ReplicatorControlEvent::ChangeRole { epoch: e, role: r, reply } => {
                        epoch = e;
                        role = r;
                        state.write_status.store(
                            compute_write_status(role, &members).into(),
                            Ordering::Release,
                        );
                        let _ = reply.send(Ok(()));
                    }
                    // ... other control events
                    _ => {}
                }
            }

            Some(req) = data_rx.recv(), if role == Role::Primary => {
                let lsn = next_lsn;
                next_lsn += 1;
                // Send to secondaries, collect ACKs, reply when quorum met
                let _ = req.reply.send(Ok(lsn));
            }

            else => break,
        }
    }
}
```

**Key design points:**
- `biased` in `tokio::select!` gives control events priority — a
  `ChangeRole(Secondary)` immediately stops processing data requests
- Data receives are guarded by `if role == Role::Primary` — secondaries
  don't process replicate requests
- All mutable state (`role`, `epoch`, `members`, `next_lsn`) is owned by
  the actor — no `Arc<Mutex<...>>`
- `PartitionState` atomics are written by the actor, read by everyone else

### Error Types

The crate uses concrete error types (not `Box<dyn Error>`) so callers can
pattern-match on specific conditions:

```rust
/// Errors returned by Replicator and StateReplicator operations.
#[derive(Debug, thiserror::Error)]
pub enum KubelicateError {
    /// This replica is not the primary. Redirect client.
    #[error("not primary")]
    NotPrimary,

    /// Primary does not have write quorum. Retry later.
    #[error("no write quorum")]
    NoWriteQuorum,

    /// Reconfiguration in progress. Retry later.
    #[error("reconfiguration pending")]
    ReconfigurationPending,

    /// Operation from a stale epoch was rejected.
    #[error("stale epoch: got {got:?}, current {current:?}")]
    StaleEpoch { got: Epoch, current: Epoch },

    /// Operation was cancelled via CancellationToken.
    #[error("cancelled")]
    Cancelled,

    /// The replica/partition is closed or shutting down.
    #[error("closed")]
    Closed,

    /// Internal error (IO, serialization, etc.)
    #[error(transparent)]
    Internal(#[from] Box<dyn std::error::Error + Send + Sync>),
}

/// Convenience alias used throughout the crate.
pub type Result<T> = std::result::Result<T, KubelicateError>;
```

**Usage in user code:**

```rust
match replicator.replicate(data, token).await {
    Ok(lsn) => { /* success */ }
    Err(KubelicateError::NotPrimary) => { /* redirect client */ }
    Err(KubelicateError::NoWriteQuorum) => { /* retry after backoff */ }
    Err(KubelicateError::ReconfigurationPending) => { /* retry */ }
    Err(KubelicateError::Cancelled) => { /* shutting down */ }
    Err(e) => { /* unexpected error */ }
}
```

---

## Partition Access Status

### AccessStatus Enum

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenMode {
    /// New replica — no previous state. Initialize fresh.
    New,
    /// Existing replica — has persisted state from a previous run.
    /// Replicator should restore from local storage.
    Existing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessStatus {
    /// Operation is allowed.
    Granted,
    /// Reconfiguration in progress. Retry later.
    ReconfigurationPending,
    /// This replica is not primary. Redirect client.
    NotPrimary,
    /// Primary but no write quorum. Reads may still work.
    NoWriteQuorum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultType {
    /// Restart replica, preserve state.
    Transient,
    /// Drop replica and rebuild from scratch.
    Permanent,
}
```

### Access Status State Machine

The replicator library computes read and write status based on two axes:
**(1) lifecycle state** and **(2) reconfiguration state + type**.

#### Normal Operation (no reconfiguration)

| Lifecycle | ReadStatus | WriteStatus |
|---|---|---|
| Opening Primary | ReconfigurationPending | ReconfigurationPending |
| Ready Primary | **Granted** | **Granted** (if quorum available, else NoWriteQuorum) |
| Secondary / Idle / None | NotPrimary | NotPrimary |

WriteStatus on a ready primary is **dynamic** — if enough secondaries go
down that write quorum is lost, it transitions to NoWriteQuorum in real time
without any reconfiguration. It returns to Granted when quorum is restored.

#### During Failover (S→P promotion)

| Reconfig Phase | ReadStatus | WriteStatus | Rationale |
|---|---|---|---|
| Phase1 (GetLSN) + Phase2 (Catchup) | ReconfigurationPending | ReconfigurationPending | Data may be stale |
| Phase2 completed | **Granted** | ReconfigurationPending | Safe to read after catchup |
| Phase4 (Activate) completed | Granted | **Granted** | Fully promoted |

#### During Switchover (P→S demotion on old primary)

| Reconfig Phase | ReadStatus | WriteStatus | Rationale |
|---|---|---|---|
| Phase0 PreCatchup | Granted | **Granted** (PC+CC quorum) | Double-catchup: writes still flowing |
| Phase0 Catchup | Granted | **ReconfigurationPending** | Write revoked; reads still OK |
| Phase0 Catchup completed | **ReconfigurationPending** | ReconfigurationPending | About to lose primary role |
| Role changed (now Secondary) | NotPrimary | NotPrimary | Permanent |

#### During No-Primary-Change Reconfiguration (adding/removing secondaries)

| Phase | ReadStatus | WriteStatus |
|---|---|---|
| Catchup in progress | Granted | Granted (dual quorum: PC+CC) |
| Completed | Granted | Granted (CC quorum only) |

No disruption to client requests.

### How the Replicator Enforces Access Status

The replicator checks WriteStatus **inside `replicate()`** before sending
operations to secondaries:

```
replicate(data, token):
  lock.read():
    status = compute_write_status()
    if status != Granted:
      return Err(status.to_error())
    // proceed with quorum replication
```

This is the authoritative gate. User-level `partition.write_status()` checks
are an optimization to fail fast before entering the replicator.

---

## Epoch and Fencing Protocol

### Epoch Structure

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Epoch {
    /// Incremented when write quorum is lost (data loss event).
    pub data_loss_number: i64,
    /// Incremented on every reconfiguration.
    pub configuration_number: i64,
}
```

Epochs use **lexicographic ordering** (data_loss_number first). This is the
primary fencing mechanism.

### Fencing Rules

1. **Operator increments epoch** on every reconfiguration (failover,
   switchover, or secondary change).
2. **Operator writes new epoch to CRD** before issuing any gRPC calls
   (write-ahead — CRD is the commit point).
3. **Operator fences ALL secondaries first** via `update_epoch(new_epoch)`
   BEFORE activating the new primary. This is the **fence-before-promote**
   discipline — secondaries reject stale operations before the new primary
   starts writing.
4. **New primary** receives its epoch via `change_role(epoch, Primary)`.
5. **Secondaries reject operations from any primary with an older epoch.**
   This prevents a zombie primary (network-partitioned old primary that
   hasn't received its demotion yet) from corrupting state.

```
Pod receives replication operation:
  if operation.epoch < self.current_epoch:
    reject(StaleEpoch)
  else:
    apply(operation)
```

### Secondary Truncation on Epoch Change

When a secondary receives `update_epoch()` with a new epoch, it must
**truncate any uncommitted operations** back to its last known committed LSN.

This handles the case where a zombie primary sent operations that were
received by some secondaries but never committed (< write quorum ACKed).
Without truncation, the secondary would hold divergent data at LSNs that
the new primary may assign different content to.

```
Secondary receives update_epoch(new_epoch):
  1. Set current_epoch = new_epoch
  2. Truncate state to committed_lsn
     (discard any operations with lsn > committed_lsn)
  3. Reject future operations with epoch < new_epoch
```

The replicator must track **two LSN values** even in the MVP:
- `committed_lsn` — highest LSN acknowledged by write quorum (safe)
- `received_lsn` — highest LSN received from primary (may be ahead of
  committed if the primary sent operations that haven't reached quorum yet)

### Kubernetes-Level Fencing

In addition to epoch-based fencing at the replicator level, the operator
uses Kubernetes mechanisms:

- **Pod labels** — primary pod labeled `kubelicate.io/role=primary`;
  `-rw` Service selector matches this label. On failover, operator updates
  labels → Kubernetes endpoint controller reroutes traffic.
- **Pod deletion as last resort** — if a zombie primary cannot be reached
  to demote, the operator deletes the pod. Kubernetes recreates it, and it
  starts fresh as an idle secondary.

### Primary Self-Fencing on Network Isolation

Epoch fencing protects secondaries from accepting stale writes, but it
requires the operator to reach secondaries with the new epoch. In an
asymmetric network partition where the old primary can reach some secondaries
but the operator cannot, epoch fencing alone is insufficient.

The primary must **self-fence** — detect its own isolation and stop accepting
writes without waiting for the operator.

**CNPG's approach (liveness probe self-kill):**
CNPG solves this by extending the primary's liveness probe. If the primary
cannot reach BOTH the Kubernetes API server AND any peer instance, the
liveness probe fails. Kubelet restarts the pod after `failureThreshold`
consecutive failures (~30s default). The restarted pod joins as idle
secondary. CNPG does NOT use Kubernetes Lease objects for this — the peer
reachability check is a direct HTTPS call to each instance's `/failsafe`
endpoint using the PostgreSQL server CA cert. (See the CNPG background doc
for full details.)

**Two options for our design:**

| Approach | Mechanism | Pros | Cons |
|---|---|---|---|
| **A: Liveness self-kill** (CNPG-style) | Replicator extends liveness probe. If isolated → probe fails → Kubelet restarts pod → rejoins as idle | Simple, proven, no new components | Pod restart is disruptive; recovery is slow (full rebuild); brief unavailability even if partition heals quickly |
| **B: Lease self-revoke** | Primary periodically renews a Kubernetes Lease. If renewal fails → replicator sets WriteStatus=NoWriteQuorum, stays alive | Graceful: no pod restart; fast recovery when partition heals (just re-grant WriteStatus); primary can still serve reads | More complex; Lease is a new dependency; must handle Lease API server being the unreachable component |

**Recommendation:** Start with **Option A (liveness self-kill)** for the MVP
— it's simpler and mirrors CNPG's proven approach. Option B can be layered on
later as an optimization for environments where pod restarts are expensive.

**Implementation (Option A):**
The replicator library provides a liveness probe HTTP handler that, on the
primary, performs:
1. Attempt to reach the Kubernetes API server (GET on a lightweight endpoint)
2. If unreachable, attempt to reach any peer replicator (HTTPS to each
   secondary's status endpoint)
3. If BOTH unreachable → return HTTP 500 (liveness failure)
4. Kubelet restarts the pod after `failureThreshold` consecutive failures

**Skipped for:** Single-replica partitions (no peers to check), secondary
replicas (no benefit to restarting an isolated secondary).

---

## Operator Workflow Sequences

These are the exact API call sequences the operator must execute for each
lifecycle operation. Derived from the SF mock test driver in
`service-fabric-rs/crates/libs/util/src/mock/stateful.rs` which faithfully
reproduces the SF runtime's call ordering.

### Create Service (Initial Partition Setup)

Creates a new replica set with N replicas. The first replica becomes primary;
others are built as secondaries.

```
Operator                          Pod-1 (will be P)    Pod-2 (will be S)
   │                                  │                     │
   │  ── For each replica: ──────────────────────────────────────────
   │  1. Factory::create_replica()    │                     │
   │     (creates service instance)   │                     │
   │                                  │                     │
   │  ── For each replica: ──────────────────────────────────────────
   │  2. Replica::open(New, partition) │                    │
   │     → returns IPrimaryReplicator │                     │
   │                                  │                     │
   │  ── For each replicator: ───────────────────────────────────────
   │  3. Replicator::open()           │                     │
   │     → returns replicator address │                     │
   │                                  │                     │
   │  ── Assign roles to replicators (replicator BEFORE replica): ──
   │  4a. Replicator[1]::change_role(epoch, Primary)        │
   │  4b. Replicator[2]::change_role(epoch, IdleSecondary)  │
   │                                  │                     │
   │  ── Assign roles to replicas: ─────────────────────────────────
   │  5a. Replica[1]::change_role(Primary)    → addr       │
   │  5b. Replica[2]::change_role(IdleSecondary) → addr    │
   │                                  │                     │
   │  ── Build each secondary via primary's replicator: ────────────
   │  6. Primary.Replicator::build_replica(                 │
   │       ReplicaInfo {                                    │
   │         id: 2,                                         │
   │         role: IdleSecondary,                           │
   │         current_progress: -1,     // unknown           │
   │         catch_up_capability: -1,  // unknown           │
   │         must_catch_up: false,                          │
   │       })                                               │
   │     [Copy stream: primary sends full state to idle]    │
   │                                  │                     │
   │  7. Replica[2]::change_role(ActiveSecondary)           │
   │     [Idle promoted to active after build completes]    │
   │                                  │                     │
   │  ── For each built secondary, update configuration: ───────────
   │  8. Primary.Replicator::update_catch_up_replica_set_configuration(
   │       current: { replicas: [Pod-2], write_quorum: 2 },
   │       previous: { replicas: [],     write_quorum: 1 })
   │                                  │                     │
   │  9. Primary.Replicator::wait_for_catch_up_quorum(Write)
   │     [Wait for Pod-2 to catch up to committed LSN]      │
   │                                  │                     │
   │  10. Primary.Replicator::update_current_replica_set_configuration(
   │       current: { replicas: [Pod-2], write_quorum: 2 })
   │     [Transition complete — single config]              │
   │                                  │                     │
   │  ── Grant access status: ──────────────────────────────────────
   │  11. Pod-1 partition: ReadStatus=Granted, WriteStatus=Granted
   │      Pod-2 partition: ReadStatus=NotPrimary, WriteStatus=NotPrimary
```

**Key observations:**
- Replicator `change_role` is called **before** replica `change_role`
  (system sets up replication plumbing before user code is notified)
- `build_replica` is called on the primary's replicator, not on the
  secondary — the primary drives the copy stream
- Configuration updates happen **one secondary at a time** — each built
  secondary goes through the full `update_catch_up → wait_for_catch_up
  → update_current` cycle before the next one is added
- Write quorum grows incrementally as secondaries join

### Restart Secondary (Graceful)

Gracefully restarts a secondary replica. The old replica is closed, a new one
is created from the same factory, opened with `OpenMode::Existing` (preserving
persisted state), and rebuilt.

```
Operator                          Primary (P)         Secondary (S, restarting)
   │                                  │                     │
   │  ── Remove from configuration: ────────────────────────────────
   │  1. Primary.Replicator::update_current_replica_set_configuration(
   │       current: { replicas: [other secondaries],        │
   │                  write_quorum: <unchanged> })           │
   │     [Quorum size preserved — graceful restart]         │
   │                                  │                     │
   │  ── Close old replica: ────────────────────────────────────────
   │  2. Replica[S]::close()          │                     │
   │  3. Replicator[S]::close()       │                     ✗
   │                                  │
   │  ── Create new replica from same factory: ─────────────────────
   │  4. Factory::create_replica(same_id)                   │(new)
   │  5. Replica::open(Existing, partition)                  │
   │     → returns replicator                               │
   │  6. Replicator::open() → addr                          │
   │                                  │                     │
   │  ── Assign idle role: ─────────────────────────────────────────
   │  7. Replicator::change_role(epoch, IdleSecondary)      │
   │  8. Replica::change_role(IdleSecondary)                │
   │                                  │                     │
   │  ── Rebuild via primary: ──────────────────────────────────────
   │  9. Primary.Replicator::build_replica(                 │
   │       ReplicaInfo {                                    │
   │         id: <same_id>,                                 │
   │         role: IdleSecondary,                           │
   │         current_progress: -1,                          │
   │         catch_up_capability: -1,                       │
   │         must_catch_up: false,                          │
   │       })                                               │
   │                                  │                     │
   │  10. Replica::change_role(ActiveSecondary)             │
   │                                  │                     │
   │  ── Re-add to configuration: ──────────────────────────────────
   │  11. Primary.Replicator::update_catch_up_replica_set_configuration(
   │        current: { ..., replicas: [+S], write_quorum: N },
   │        previous: { old config })
   │  12. Primary.Replicator::wait_for_catch_up_quorum(Write)
   │  13. Primary.Replicator::update_current_replica_set_configuration(
   │        current: { full set, write_quorum: N })
```

**Key observations:**
- `OpenMode::Existing` is used instead of `New` — the replica may have
  persisted state from before the restart
- The replica ID is **reused** (same identity, new instance)
- Quorum size is preserved during the restart (not reduced) since this is a
  graceful restart where we know the replica will come back
- The full build → catch-up → config-update cycle runs even for a restart,
  because the replica may have missed operations while down

### Delete Service (Partition Teardown)

Gracefully shuts down all replicas in the partition.

```
Operator                          Primary (P)         Secondaries (S)
   │                                  │                     │
   │  ── Revoke all access: ────────────────────────────────────────
   │  1. All partitions: ReadStatus=ReconfigurationPending
   │                     WriteStatus=ReconfigurationPending
   │                                  │                     │
   │  ── Demote primary: ──────────────────────────────────────────
   │  2. Replica[P]::change_role(ActiveSecondary)           │
   │  3. Replicator[P]::change_role(epoch, ActiveSecondary) │
   │                                  │                     │
   │  ── Change all to None: ──────────────────────────────────────
   │  4. For each replica:            │                     │
   │     Replica::change_role(None)   │                     │
   │     Replicator::change_role(epoch, None)               │
   │                                  │                     │
   │  ── Close all: ───────────────────────────────────────────────
   │  5. For each replica:            │                     │
   │     Replica::close()             │                     │
   │     Replicator::close()          ✗                     ✗
```

**Key observations:**
- Access status is revoked **first**, before any role changes — clients see
  ReconfigurationPending immediately
- Primary is demoted to ActiveSecondary **before** being changed to None
  (it cannot go directly P→None; the intermediate step ensures clean
  write revocation)
- The delete ordering for primary is: `Replica::change_role` **then**
  `Replicator::change_role` (reverse of the creation order — this is the
  demotion ordering, not the promotion ordering)

---

## Failover Sequence

Unplanned primary failure (primary pod crashes or becomes unreachable).

```
Time ─────────────────────────────────────────────────────────────►

Operator                          Pod-1 (was P)    Pod-2 (S)   Pod-3 (S)
   │                                  ✗ (down)        │           │
   │                                                   │           │
1. Detect primary down                                 │           │
   (pod not reporting status)                          │           │
   │                                                   │           │
2. Wait failoverDelay                                  │           │
   │                                                   │           │
3. Query LSNs from surviving pods                      │           │
   │──── GET /status ─────────────────────────────────►│           │
   │──── GET /status ─────────────────────────────────────────────►│
   │◄─── LSN=142 ─────────────────────────────────────│           │
   │◄─── LSN=140 ─────────────────────────────────────────────────│
   │                                                   │           │
4. Select Pod-2 (highest LSN=142)                      │           │
   │                                                   │           │
5. Update CRD status (WRITE-AHEAD — CRD commit before any gRPC):
   targetPrimary: Pod-2                                │           │
   epoch.configurationNumber++                         │           │
   phase: FailingOver                                  │           │
   reconfigurationPhase: Phase1_GetLSN                 │           │
   │                                                   │           │
6. Fence ALL secondaries: update_epoch(new_epoch)      │           │
   │──── UpdateEpoch ─────────────────────────────────►│           │
   │──── UpdateEpoch ─────────────────────────────────────────────►│
   │                               All secondaries now │           │
   │                               reject ops from old │           │
   │                               epoch               │           │
   │                                                   │           │
7. Signal Pod-2: change_role(new_epoch, Primary)       │           │
   │──── DoReconfiguration ──────────────────────────►│           │
   │                                                   │           │
   │                               Pod-2 executes:     │           │
   │                               - change_role(P)    │           │
   │                               - wait_for_catchup  │           │
   │                                                   │──Repl──►│
   │                                                   │◄─ACK────│
   │                                                   │           │
8. Pod-2 reports: catchup complete                     │           │
   │◄─── Phase2_Done ────────────────────────────────│           │
   │                                                   │           │
9. Operator activates:                                 │           │
   Update CRD: phase=Healthy                           │           │
   Update Pod labels: Pod-2 → role=primary             │           │
   │                                                   │           │
10. Pod-2: WriteStatus = Granted                       │           │
    Begins accepting writes                            │           │
   │                                                   │           │
11. Pod-1 restarts, receives update_epoch(new_epoch)   │           │
    Truncates to committed LSN, joins as secondary     │           │
```

---

## Switchover Sequence

Planned primary change (upgrade, rebalancing, user request).

```
Operator                          Pod-1 (P)        Pod-2 (S, target)
   │                                  │                │
1. Set targetPrimary = Pod-2          │                │
   epoch.configurationNumber++        │                │
   reconfigurationPhase: Phase0       │                │
   │                                  │                │
2. Signal Pod-1: begin demotion       │                │
   │──── PreCatchup ─────────────────►│                │
   │                                  │                │
   │              Pod-1 WriteStatus:  │                │
   │              still Granted       │                │
   │              (double-catchup     │                │
   │               with writes)       │                │
   │                                  │──Repl────────►│
   │                                  │                │
3. Pod-1 reports pre-catchup done     │                │
   │◄──── PreCatchupDone ────────────│                │
   │                                  │                │
4. Operator: revoke write status      │                │
   │──── RevokeWrite ────────────────►│                │
   │                                  │                │
   │              Pod-1 WriteStatus:  │                │
   │              ReconfigPending     │                │
   │              ReadStatus: Granted │                │
   │                                  │                │
5. Final catchup (no new writes)      │                │
   │              Pod-1 drains last   │                │
   │              operations to Pod-2 │──Repl────────►│
   │                                  │                │
6. Pod-1 reports catchup done         │                │
   │◄──── CatchupDone ──────────────│                │
   │                                  │                │
   │              Pod-1 ReadStatus:   │                │
   │              ReconfigPending     │                │
   │                                  │                │
7. Operator signals role change       │                │
   │──── ChangeRole(S) ─────────────►│                │
   │──── ChangeRole(P) ──────────────────────────────►│
   │                                  │                │
   │              Pod-1: NotPrimary   │                │
   │              Pod-2: Granted      │                │
   │                                  │                │
8. Update labels and CRD              │                │
   phase: Healthy                     │                │
```

---

## Reconfiguration Phases

The operator drives reconfiguration as a state machine through the CRD
status. Each phase is a checkpoint — the operator can resume from any phase
after a restart.

| Phase | Type | What Happens |
|---|---|---|
| **Phase0_Demote** | Swap only | Old primary: pre-catchup with writes → revoke write → final catchup → revoke read → demote |
| **Phase1_GetLSN** | Failover | Operator queries surviving replicas for their LSN to select the best candidate |
| **Phase2_Catchup** | Both | New primary waits for secondaries to catch up. `wait_for_catch_up_quorum()` |
| **Phase3_Deactivate** | Both | Old configuration deactivated. Epoch boundary applied |
| **Phase4_Activate** | Both | New primary activated with `change_role(Primary)`. WriteStatus → Granted |

All phases are **idempotent** — the operator can re-execute any phase safely
after a crash. The CRD status records the current phase, so the operator
picks up where it left off.

---

## Replication Data Path

The `ReplicatorControl` gRPC service (described in Operator ↔ Pod
Communication) handles **control plane** commands. The **data plane** —
actual replication of user writes from primary to secondaries — uses a
separate transport.

### Normal Operation Write Flow

```
User code                 Replicator (Primary)              Replicator (Secondary)
   │                           │                                 │
   │── replicate(data) ──────►│                                 │
   │                           │                                 │
   │                           │  1. Assign LSN                  │
   │                           │  2. Persist to local WAL (*)    │
   │                           │  3. Send to each configured     │
   │                           │     secondary (from ReplicaSet- │
   │                           │     Config.members):             │
   │                           │                                 │
   │                           │── ReplicationData(epoch,lsn,──►│
   │                           │   data) via gRPC stream         │
   │                           │                                 │
   │                           │                          4. Validate epoch
   │                           │                             (reject if stale)
   │                           │                          5. Persist to local WAL (*)
   │                           │                          6. Apply to state provider
   │                           │                                 │
   │                           │◄── ACK(lsn) ───────────────────│
   │                           │                                 │
   │                           │  7. Count ACKs                  │
   │                           │     If ACKs >= writeQuorum:     │
   │                           │     → commit locally            │
   │                           │     → return LSN to user        │
   │                           │                                 │
   │◄── Ok(lsn) ─────────────│                                 │
```

**(\\*) MVP simplification:** Steps 2 and 5 (replicator WAL persistence) are
skipped in the initial implementation. Instead, step 6 (apply to state
provider) must complete **synchronously and durably** before the ACK is sent.
The state provider is the durability layer — ACK means persisted.
See [MVP Simplifications](#mvp-simplifications).

### Data Plane Transport

Each primary↔secondary pair maintains a **persistent gRPC bidirectional
stream** for replication:

```protobuf
service ReplicatorData {
    // Primary streams operations to secondary; secondary streams ACKs back
    rpc ReplicationStream(stream ReplicationItem)
        returns (stream ReplicationAck);
}

message ReplicationItem {
    int64 epoch_data_loss = 1;
    int64 epoch_config = 2;
    int64 lsn = 3;
    bytes data = 4;
}

message ReplicationAck {
    int64 lsn = 1;
}
```

The primary replicator creates one stream per secondary in
`ReplicaSetConfig.members`. When configuration changes (via
`update_catch_up_replica_set_configuration`), the replicator adds/removes
streams accordingly.

### Secondary-Side Processing

Replicated operations are processed **entirely inside the replicator** — the
user's `ServiceEvent` channel does NOT receive replication data. The
replicator:

1. Receives `ReplicationItem` on the gRPC stream
2. Validates epoch (rejects stale epochs from zombie primaries)
3. Passes data to the `StateProvider` (internal replicator component) which
   persists it and applies it to state
4. Sends ACK back to primary

The user's secondary code (if any) reads state through read-only access to
the same state store. The replicator owns the state lifecycle — user code on
secondaries is a passive observer.

This means the `ServiceEvent::ChangeRole { new_role: ActiveSecondary }` does
NOT need to set up any replication handling — the replicator does it all.

---

## Quorum Calculation

### Write Quorum Formula

```
Write Quorum = ⌊N/2⌋ + 1

Where N = total replicas in the configuration (including primary)
```

| Total Replicas | Write Quorum | Tolerated Failures |
|---|---|---|
| 1 | 1 | 0 (no replication) |
| 2 | 2 | 0 (both must ACK) |
| 3 | 2 | 1 |
| 5 | 3 | 2 |
| 7 | 4 | 3 |

**The primary counts toward quorum.** When the primary persists locally, that
counts as one ACK. So with 3 replicas and writeQuorum=2, the primary needs
only 1 secondary ACK to commit.

### CRD `spec.minReplicas` vs Write Quorum

`spec.minReplicas` is the **minimum replica set size** — the operator will
not reduce the replica set below this number. It does NOT directly set the
write quorum. Write quorum is always computed as `⌊N/2⌋+1` where N is the
current active replica count.

However, `minReplicas` affects **availability**: if the active count drops
below `minReplicas`, the operator may refuse certain operations (like
removing a secondary during upgrade) to prevent quorum loss.

### Dual-Config Quorum During Reconfiguration

When both `previousConfiguration` and `currentConfiguration` exist, writes
must be acknowledged by a write quorum from **both**:

```
Commit requires:
  ACKs from CC members >= CC.writeQuorum
  AND
  ACKs from PC members >= PC.writeQuorum
```

This ensures that no committed data is lost during configuration transitions.
A replica present in both PC and CC counts toward both quorums with a single
ACK.

---

## Data Loss Detection

### When `dataLossNumber` is Incremented

The operator increments `epoch.dataLossNumber` when **write quorum is lost**
— a majority of replicas (including the primary) are down simultaneously,
and the surviving replicas may not have all committed operations.

### Decision Tree

```
Primary fails
  │
  ├── Majority of replicas still up?
  │     YES → Normal failover (increment configurationNumber only)
  │           Select replica with highest LSN as new primary
  │
  │     NO → Quorum loss detected
  │           │
  │           ├── Any replicas up at all?
  │           │     NO → Cluster is fully down. Wait for pods to restart.
  │           │           On restart, increment dataLossNumber.
  │           │
  │           │     YES → Partial survivors
  │           │           Increment dataLossNumber
  │           │           Select best surviving replica as new primary
  │           │           Call on_data_loss() on new primary's replicator
  │           │           │
  │           │           ├── Returns StateChanged → Rebuild all secondaries
  │           │           │   from new primary (their data may diverge)
  │           │           │
  │           │           └── Returns None → Proceed with normal catchup
  │           │               (survivors have consistent state)
```

### `on_data_loss()` Contract

When the replicator receives `on_data_loss()`:
- It delegates to the `StateProvider::on_data_loss()` implementation
- The state provider may: restore from backup, accept the loss, or
  reconcile state
- Returns `DataLossAction::StateChanged` if the state was modified
  (e.g., restored from backup), triggering SF-style full rebuild of other
  replicas
- Returns `DataLossAction::None` if the state is accepted as-is

---

## Operator Crash Recovery

### The Problem

The operator can crash at any point during a reconfiguration. It must be able
to resume from the persisted CRD status without repeating completed steps or
leaving the cluster in an inconsistent state.

### Recovery Strategy

The CRD status serves as a **durable state machine checkpoint**. On restart,
the operator reads:

1. `status.reconfigurationPhase` — which phase was in progress
2. `status.targetPrimary` — who should be primary
3. `status.currentConfiguration` / `status.previousConfiguration` — the
   replica set state
4. `status.epoch` — the current epoch

Then it **re-enters the state machine at the recorded phase**:

| Recorded Phase | Recovery Action |
|---|---|
| `None` | Normal reconciliation. No reconfiguration in progress. |
| `Phase1_GetLSN` | Re-query all pod LSNs. Re-select primary if needed. |
| `Phase2_Catchup` | Check if catchup completed (query primary's replicator). If not, re-issue `wait_for_catch_up_quorum()`. |
| `Phase3_Deactivate` | Check if epoch was applied. If not, re-issue `update_epoch()` to secondaries. |
| `Phase4_Activate` | Check if primary received `change_role(Primary)`. If not, re-issue. Then complete. |
| `Phase0_Demote` | Check old primary's role. Resume demotion if needed. |

### Idempotency Requirements

All operator→replicator commands must be **idempotent**:
- `change_role(epoch, Primary)` on an already-primary replica is a no-op
- `update_epoch(epoch)` with the current epoch is a no-op
- `build_replica()` on an already-built replica is a no-op (or can be
  re-triggered safely — the primary re-sends the copy stream)
- `update_catch_up_replica_set_configuration()` with the same config is
  a no-op
- `wait_for_catch_up_quorum()` on an already-caught-up quorum returns
  immediately

This makes crash recovery safe — the operator simply re-executes the current
phase, and commands that already completed are no-ops.

### CRD Status as Write-Ahead Log

The operator follows a **write-ahead** pattern:
1. Write the new phase to CRD status
2. THEN execute the phase's actions
3. On success, write the next phase to CRD status

If the operator crashes between steps 1 and 2, it re-executes step 2 on
restart (idempotent). If it crashes between steps 2 and 3, it re-executes
step 2 (which is a no-op) and then proceeds to step 3.

---

## Concurrent Reconfiguration Safety

### The Problem

What if a second failure occurs during an ongoing reconfiguration? For
example:
- Primary fails → operator starts failover → selects Pod-2 as new primary
- During Phase2 (catchup), Pod-2 also fails

### Design Rules

**Rule 1: Only one reconfiguration at a time.** The operator never starts a
new reconfiguration while one is in progress. If a new failure is detected
during reconfiguration, the operator either:
- Incorporates it into the current reconfiguration (e.g., removes the failed
  pod from the target configuration), or
- Aborts the current reconfiguration and starts a new one from scratch

**Rule 2: Abort-and-restart on candidate failure.** If the target primary
fails during failover:
1. Operator sets `reconfigurationPhase = None` (abort)
2. Operator re-evaluates all surviving pods
3. Selects a new candidate and starts a fresh reconfiguration
4. Increments `epoch.configurationNumber` again

**Rule 3: Secondary failures during reconfiguration are deferred.** If a
non-critical secondary fails during reconfiguration:
1. Operator removes it from the target `currentConfiguration`
2. Recalculates write quorum
3. Continues the current reconfiguration with fewer secondaries
4. After reconfiguration completes, handles the secondary failure as a
   separate operation (rebuild or remove)

**Rule 4: Protect against operator flapping.** The `failoverDelay`
(default 0, configurable) prevents the operator from starting a new
reconfiguration too quickly after an abort. Combined with exponential backoff
on repeated failures of the same pod.

### State Machine Transitions on Abort

```
Any Phase
    │
    ├── Target primary fails
    │     → reconfigurationPhase = None
    │     → targetPrimary = "pending"
    │     → Re-enter failover from Phase1_GetLSN
    │
    ├── Non-critical secondary fails
    │     → Remove from currentConfiguration
    │     → Continue current phase
    │
    └── Operator loses connectivity to all pods
          → Set phase = None
          → Wait and retry (cannot make progress without pod contact)
```

---

## Operator ↔ Pod Communication

### Option A: gRPC (Recommended for MVP)

Each pod runs a gRPC server (part of the replicator library) that the
operator calls:

```protobuf
syntax = "proto3";
package kubelicate.v1;

// ---- Shared types ----

message EpochProto {
    int64 data_loss_number = 1;
    int64 configuration_number = 2;
}

enum RoleProto {
    ROLE_NONE = 0;
    ROLE_PRIMARY = 1;
    ROLE_ACTIVE_SECONDARY = 2;
    ROLE_IDLE_SECONDARY = 3;
}

enum ReplicaStatusProto {
    STATUS_UP = 0;
    STATUS_DOWN = 1;
}

enum QuorumModeProto {
    QUORUM_ALL = 0;
    QUORUM_WRITE = 1;
}

message ReplicaInfoProto {
    int64 id = 1;
    RoleProto role = 2;
    ReplicaStatusProto status = 3;
    string replicator_address = 4;
    int64 current_progress = 5;
    int64 catch_up_capability = 6;
    bool must_catch_up = 7;
}

message ReplicaSetConfigProto {
    repeated ReplicaInfoProto members = 1;
    uint32 write_quorum = 2;
}

// ---- Control plane: Operator → Pod ----

service ReplicatorControl {
    // Lifecycle
    rpc ChangeRole(ChangeRoleRequest) returns (ChangeRoleResponse);
    rpc UpdateEpoch(UpdateEpochRequest) returns (UpdateEpochResponse);

    // Status reporting (operator polls this)
    rpc GetStatus(GetStatusRequest) returns (GetStatusResponse);

    // Primary-only reconfiguration commands
    rpc UpdateCatchUpConfiguration(UpdateCatchUpConfigRequest) returns (UpdateCatchUpConfigResponse);
    rpc UpdateCurrentConfiguration(UpdateCurrentConfigRequest) returns (UpdateCurrentConfigResponse);
    rpc WaitForCatchUpQuorum(WaitForCatchUpQuorumRequest) returns (WaitForCatchUpQuorumResponse);
    rpc BuildReplica(BuildReplicaRequest) returns (BuildReplicaResponse);
    rpc RemoveReplica(RemoveReplicaRequest) returns (RemoveReplicaResponse);
    rpc OnDataLoss(OnDataLossRequest) returns (OnDataLossResponse);
}

message ChangeRoleRequest {
    EpochProto epoch = 1;
    RoleProto role = 2;
}
message ChangeRoleResponse {}

message UpdateEpochRequest {
    EpochProto epoch = 1;
}
message UpdateEpochResponse {}

message GetStatusRequest {}
message GetStatusResponse {
    RoleProto role = 1;
    EpochProto epoch = 2;
    int64 current_progress = 3;     // LSN: end of log
    int64 catch_up_capability = 4;  // LSN: beginning of log
    int64 committed_lsn = 5;        // LSN: last committed
    bool healthy = 6;
}

message UpdateCatchUpConfigRequest {
    ReplicaSetConfigProto current = 1;
    ReplicaSetConfigProto previous = 2;
}
message UpdateCatchUpConfigResponse {}

message UpdateCurrentConfigRequest {
    ReplicaSetConfigProto current = 1;
}
message UpdateCurrentConfigResponse {}

message WaitForCatchUpQuorumRequest {
    QuorumModeProto mode = 1;
}
message WaitForCatchUpQuorumResponse {}

message BuildReplicaRequest {
    ReplicaInfoProto replica = 1;
}
message BuildReplicaResponse {}

message RemoveReplicaRequest {
    int64 replica_id = 1;
}
message RemoveReplicaResponse {}

message OnDataLossRequest {}
message OnDataLossResponse {
    bool state_changed = 1;
}

// ---- Data plane: Pod ↔ Pod (replication) ----

service ReplicatorData {
    // Primary streams operations to secondary; secondary streams ACKs back
    rpc ReplicationStream(stream ReplicationItem)
        returns (stream ReplicationAck);
}

message ReplicationItem {
    int64 epoch_data_loss = 1;
    int64 epoch_config = 2;
    int64 lsn = 3;
    bytes data = 4;
}

message ReplicationAck {
    int64 lsn = 1;
}
```

**Advantages:** Strongly typed, low latency, bidirectional streaming for
status updates.

### Option B: CRD Status + Annotations (Alternative)

Operator writes commands into CRD status; pod watches CRD and acts.
Pod writes its status into a ConfigMap or CRD sub-resource.

**Advantages:** No extra port, pure Kubernetes-native.
**Disadvantages:** Higher latency (watch delays), more complex state
reconciliation.

### Recommendation

Use **gRPC for latency-sensitive control plane** (reconfiguration commands,
status polling) and **CRD status for durable state** (current epoch,
configuration, phase). This mirrors how SF uses direct messaging (FM→RA) for
commands while persisting configuration in the FM's replicated store.

**Authority rule:** On operator restart, CRD status is the source of truth.
The operator re-derives the current phase from CRD status and re-issues
gRPC commands as needed (all commands are idempotent). If gRPC state and
CRD state conflict, CRD wins.

### Security: mTLS (Post-MVP)

Both gRPC services (`ReplicatorControl` and `ReplicatorData`) should use
**mutual TLS (mTLS)** in production, but this is **deferred past the MVP**.
The MVP assumes a trusted single-tenant cluster where all pods in the
namespace are cooperating.

**Post-MVP mTLS design:**

| Channel | Client Cert | Server Cert | Purpose |
|---|---|---|---|
| Operator → Pod (control) | Operator SA cert | Pod cert | Only operator can issue ChangeRole, UpdateEpoch, etc. |
| Pod → Pod (data plane) | Replica cert | Replica cert | Only configured replicas can send/receive replication data |

The operator would issue per-replica TLS certificates (stored as Kubernetes
Secrets) and rotate them on a configurable schedule.

**ReportFault rate limiting** (MVP): Even without mTLS, the operator
rate-limits fault reports per pod (e.g., max 1 permanent fault per 60
seconds) to prevent cascading failovers from misbehaving pods.

---

## User Application Programming Model

### Design Decision: Event-Based API (mpsc)

The user API is **event-based** rather than trait-based. The user receives
lifecycle events on an `mpsc::Receiver` and processes them in an event loop
with fully owned `&mut` state. No `Arc<Mutex<...>>` is needed.

**Why not an async trait?** An `async fn(&self, ...)` trait forces `Send +
Sync + 'static` bounds and requires interior mutability (`Arc<Mutex<...>>`)
for any mutable state. This is painful in Rust:

- Holding a `Mutex` guard across `.await` points is a deadlock footgun
- Every field that changes needs wrapping, even simple counters
- `async_trait` boxes every future (overhead and type-erasure)
- Concurrent calls to `change_role` + `close` require careful reasoning

The event-based approach avoids all of this:

- User owns their state with plain `&mut self`
- Events are processed sequentially — no concurrency within the handler
- Background tasks (primary work loop) are managed by the user via `tokio::spawn`
- Replies use `oneshot::Sender` — the system awaits the reply

### Service Events

```rust
/// Events delivered by the kubelicate runtime to the user service.
/// The user processes these in an event loop with owned mutable state.
pub enum ServiceEvent {
    /// Replica opened. Initialize state, store handles.
    Open {
        ctx: ServiceContext,
        reply: oneshot::Sender<Result<()>>,
    },

    /// Role changed. Start/stop background work accordingly.
    ///
    /// Reply with the new listening address (e.g., "http://0.0.0.0:8080")
    /// or empty string if not listening in this role.
    ///
    /// Ordering guarantee: the system calls Replicator::change_role()
    /// BEFORE delivering this event on promotion (S→P), and AFTER on
    /// demotion (P→S). User code doesn't need to coordinate with the
    /// replicator.
    ChangeRole {
        new_role: Role,
        reply: oneshot::Sender<Result<String>>,
    },

    /// Graceful shutdown. Drain in-flight work, flush state, release
    /// resources. The system calls Replicator::close() after this returns.
    Close {
        reply: oneshot::Sender<Result<()>>,
    },

    /// Ungraceful termination. Release resources best-effort and return
    /// immediately. No reply channel — the system doesn't wait.
    Abort,
}

/// Handles provided to the user at Open time.
pub struct ServiceContext {
    /// Query read/write access status, report faults.
    pub partition: Arc<dyn Partition>,
    /// Replicate writes to quorum (usable on primary only).
    pub replicator: Arc<dyn StateReplicator>,
    /// Cancellation token for the replica's lifetime.
    /// Cancelled when close or abort is triggered.
    pub token: CancellationToken,
}
```

### User Event Loop Pattern

```rust
async fn run_service(mut events: mpsc::Receiver<ServiceEvent>) {
    // Owned mutable state — no Arc, no Mutex
    let mut partition: Option<Arc<dyn Partition>> = None;
    let mut replicator: Option<Arc<dyn StateReplicator>> = None;
    let mut bg_handle: Option<JoinHandle<()>> = None;
    let mut bg_token: Option<CancellationToken> = None;

    while let Some(event) = events.recv().await {
        match event {
            ServiceEvent::Open { ctx, reply } => {
                partition = Some(ctx.partition);
                replicator = Some(ctx.replicator);
                let _ = reply.send(Ok(()));
            }

            ServiceEvent::ChangeRole { new_role, reply } => {
                // Cancel any running background work
                if let Some(token) = bg_token.take() {
                    token.cancel();
                }
                if let Some(handle) = bg_handle.take() {
                    let _ = handle.await;
                }

                let addr = match new_role {
                    Role::Primary => {
                        // Start background work loop
                        let p = partition.clone().unwrap();
                        let r = replicator.clone().unwrap();
                        let token = CancellationToken::new();
                        let child = token.child_token();
                        bg_token = Some(token);
                        bg_handle = Some(tokio::spawn(async move {
                            primary_loop(p, r, child).await;
                        }));
                        "http://0.0.0.0:8080".to_string()
                    }
                    _ => String::new(),
                };
                let _ = reply.send(Ok(addr));
            }

            ServiceEvent::Close { reply } => {
                // Cancel background work
                if let Some(token) = bg_token.take() {
                    token.cancel();
                }
                if let Some(handle) = bg_handle.take() {
                    let _ = handle.await;
                }
                let _ = reply.send(Ok(()));
                break; // Exit event loop
            }

            ServiceEvent::Abort => {
                if let Some(token) = bg_token.take() {
                    token.cancel();
                }
                break; // Exit immediately
            }
        }
    }
}
```

### Primary Work Loop

The background task runs on the primary, using the partition and replicator
handles from `ServiceContext`:

```rust
async fn primary_loop(
    partition: Arc<dyn Partition>,
    replicator: Arc<dyn StateReplicator>,
    token: CancellationToken,
) {
    while !token.is_cancelled() {
        match partition.write_status() {
            AccessStatus::Granted => {
                let data = get_next_work_item().await;
                match replicator.replicate(data, token.clone()).await {
                    Ok(_lsn) => { /* apply locally */ }
                    Err(e) if is_not_primary(&e) => break,
                    Err(e) if is_transient(&e) => {
                        tokio::time::sleep(BACKOFF).await;
                    }
                    Err(_) => break,
                }
            }
            AccessStatus::NoWriteQuorum => {
                tokio::time::sleep(BACKOFF).await;
            }
            _ => break, // NotPrimary or ReconfigurationPending
        }
    }
}
```

### Request Handling (on Primary)

For services that also serve client requests (HTTP/gRPC), the request handler
runs concurrently with the event loop — it uses the `Arc<dyn Partition>` and
`Arc<dyn StateReplicator>` handles directly:

```rust
async fn handle_write(
    partition: &dyn Partition,
    replicator: &dyn StateReplicator,
    key: String,
    value: Bytes,
) -> Result<()> {
    // Fast path check
    if partition.write_status() != AccessStatus::Granted {
        return Err(access_error(partition.write_status()));
    }

    // Replicate (replicator checks status again — authoritative gate)
    let lsn = replicator
        .replicate(value.clone(), CancellationToken::new())
        .await?;

    // Apply to local state after quorum ack
    apply_locally(key, value, lsn).await
}

async fn handle_read(partition: &dyn Partition, key: &str) -> Result<Bytes> {
    if partition.read_status() != AccessStatus::Granted {
        return Err(access_error(partition.read_status()));
    }
    read_locally(key).await
}
```

### Wiring at Pod Startup

```rust
#[tokio::main]
async fn main() -> Result<()> {
    let (event_tx, event_rx) = mpsc::channel(16);

    // Start the kubelicate runtime (manages replicator + operator comms)
    let runtime = KubelicateRuntime::builder()
        .replicator(WalReplicator::new(config))
        .event_sender(event_tx)
        .build();

    // Run user service and runtime concurrently
    tokio::select! {
        result = runtime.run() => result?,
        _ = run_service(event_rx) => {},
    }

    Ok(())
}
```

The runtime:
1. Creates and opens the replicator
2. Listens for operator gRPC commands
3. On lifecycle events, sends `ServiceEvent` to the user's channel
4. Awaits the `oneshot` reply with a **configurable timeout** (default 30s)
   - On timeout: treats as abort — drops the event channel, calls
     `Replicator::abort()`, responds to operator with error
   - This prevents blocked/panicked user handlers from wedging the
     reconfiguration pipeline
5. Routes replicator-internal commands (build_replica, update_config, etc.)
   directly to the replicator — the user never sees these

**Backpressure:** The event channel is bounded (`mpsc::channel(16)`). If the
user's event loop falls behind, the runtime's `send()` blocks. To prevent
this from deadlocking operator status polling, the runtime uses
`send_timeout()` with a configurable limit. On channel full timeout, the
runtime treats it as a stuck handler (same as oneshot timeout above).

**gRPC call timeouts:** All operator → pod gRPC calls have a configurable
timeout (default 30s). If a pod doesn't respond, the operator marks it as
unresponsive and may initiate failover. This prevents slow pods from blocking
the operator's reconciliation loop.

### Error Handling Rules

| Error from `replicate()` | Meaning | Action |
|---|---|---|
| `NotPrimary` | Lost primary role | Stop work, exit primary loop. `ChangeRole` event coming. |
| `ReconfigurationPending` | Reconfiguration in progress | Retry after backoff |
| `NoWriteQuorum` | Not enough secondaries for quorum | Retry after backoff |
| `StaleEpoch` | Zombie primary detected | Stop work immediately |

### ReportFault

```rust
// Detected corrupted local state
partition.report_fault(FaultType::Permanent);
// → Operator drops this pod, creates a new one, builds from other replica

// Detected transient issue (e.g., resource exhaustion)
partition.report_fault(FaultType::Transient);
// → Operator restarts this pod, preserving persisted state
```

---

## Degenerate Configurations

### Single Replica (replicas=1)

With one replica, there is no replication, no quorum, and no failover target.

| Aspect | Behavior |
|---|---|
| WriteStatus | Granted (write quorum = 1, satisfied by primary alone) |
| Failover | **Not possible.** If the pod dies, the partition is unavailable until the pod restarts. |
| Recovery | Pod restarts with `OpenMode::Existing`, resumes as primary. No `build_replica` needed. |
| Isolation check | **Skipped** — no peers to check (matches CNPG behavior). |
| Data loss | Any pod crash with uncommitted state provider writes loses data. |

**Recommendation:** `replicas=1` is supported for development and testing
only. Production deployments should use `replicas >= 3`.

### Two Replicas (replicas=2)

With two replicas, write quorum = 2. Both replicas must ACK every write.

| Aspect | Behavior |
|---|---|
| WriteStatus | Granted only when both replicas are up. If either fails → NoWriteQuorum. |
| Failover | The surviving secondary has **all committed data** (every commit required its ACK). Failover is safe — do NOT increment `dataLossNumber`. |
| Tolerance | Zero write fault tolerance. Any single failure blocks writes. |
| Data loss | Only on simultaneous double failure. |

**Recommendation:** `replicas=2` provides read availability during single
failures but zero write availability. Use `replicas >= 3` for write
fault tolerance.

### Pod Anti-Affinity

The operator should configure **pod anti-affinity** by default to ensure
replicas are spread across failure domains:

```yaml
# Auto-injected by operator into pod spec
affinity:
  podAntiAffinity:
    preferredDuringSchedulingIgnoredDuringExecution:
      - weight: 100
        podAffinityTerm:
          labelSelector:
            matchLabels:
              kubelicate.io/set: my-app
          topologyKey: kubernetes.io/hostname
```

This ensures the quorum model's assumption of failure independence holds in
practice. Without anti-affinity, all replicas could land on the same node,
making the quorum tolerance meaningless.

---

## Build Replica Idempotency

`build_replica` is claimed to be idempotent for operator crash recovery, but
**partial copies require cleanup before retry**:

- If a `build_replica` is interrupted (operator crash, network failure, pod
  restart), the idle secondary may have received a partial copy stream
- Re-triggering `build_replica` on the same secondary must either:
  a. **Discard partial state** and restart the copy from scratch, or
  b. **Resume** from a checkpointed offset (complex, not MVP)

**MVP approach:** On `build_replica` retry, the replicator discards any
partial copy state and restarts the full copy. The idle secondary must be
prepared to receive a new copy stream that overwrites any partial data.

**Operator behavior:** If `build_replica` fails or times out (configurable,
default 5 minutes), the operator:
1. Calls `remove_replica(id)` on the primary
2. Deletes the idle pod
3. Creates a new pod and retries from scratch

---

## Reconfiguration Abort Behavior

When a reconfiguration is aborted (e.g., target primary fails during
catchup), the operator must **demote any partially-promoted candidate**:

```
Abort sequence:
  1. If candidate received change_role(Primary):
     → Send change_role(epoch, ActiveSecondary) to demote it
     → Candidate's replicator stops accepting writes
  2. Set CRD: reconfigurationPhase = None, targetPrimary = "pending"
  3. Increment epoch.configurationNumber (to fence any operations from
     the aborted reconfiguration)
  4. Fence all secondaries with new epoch: update_epoch(new_epoch)
  5. Re-enter failover from Phase1_GetLSN with remaining healthy pods
```

Without step 1, a partially-promoted candidate continues believing it's
primary, creating a split-brain with whatever primary the operator selects
next.

---

## Access Status: Phase3_Deactivate

The access status tables were missing the Phase3_Deactivate transition.
Updated tables:

**During Failover (S→P promotion) — complete table:**

| Reconfig Phase | ReadStatus | WriteStatus | Rationale |
|---|---|---|---|
| Phase1 (GetLSN) | ReconfigurationPending | ReconfigurationPending | Selecting candidate |
| Phase2 (Catchup) | ReconfigurationPending | ReconfigurationPending | Data may be stale |
| Phase2 completed | **Granted** | ReconfigurationPending | Safe to read after catchup |
| Phase3 (Deactivate) | Granted | ReconfigurationPending | Deactivating old config |
| Phase4 (Activate) completed | Granted | **Granted** | Fully promoted |

---

## Epoch Type Migration

The older `kubelicate-data/src/types.rs` (reference code) defines:

```rust
pub type Epoch = i64;  // Old: single integer
```

The new `kubelicate-core/src/types.rs` uses the structured epoch:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Epoch {
    pub data_loss_number: i64,
    pub configuration_number: i64,
}
```

The `kubelicate-core` crate is the canonical source of types going forward.
The `kubelicate-data` crate is retained as reference code only.

---

## Open Questions

1. **Multi-partition support** — How to extend to multiple partitions per
   KubelicateSet? Each partition would need its own independent replica set
   and failover state. This is future work.

2. **Pod identity and PVC binding** — When a pod is deleted and recreated,
   how to bind it to the correct PVC? Options: operator manages PVCs directly
   (like CNPG), or use a naming convention.

3. **Replication transport** — Pod-to-pod replication currently uses gRPC.
   Should we explore QUIC or direct TCP for lower overhead?

4. **Integration with `kubelicate-core`** — The `kubelicate-core` crate
   contains all types from this design (`Epoch`, `Role`, `AccessStatus`,
   `ReplicaSetConfig`, etc.). The `Replicator` trait, `Partition` trait,
   and `ServiceEvent` enum will be added to `kubelicate-core` as
   implementation proceeds. The older `kubelicate-data` crate is retained
   as reference code — its `StateProvider`, `WalStorage`, `OperationSender`
   / `OperationReceiver` traits may inform specific replicator
   implementations (e.g., `WalReplicator`) but are not the canonical API.
