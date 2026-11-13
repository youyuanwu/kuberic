# Service Fabric: API Surface and References

Rust API bindings, comparison with CloudNativePG, and key
source code references.

> Part of the [SF Architecture Reference](README.md).

---

## Rust API Surface (service-fabric-rs)

The Rust bindings in `service-fabric-rs` expose the SF COM APIs as async
traits. The key traits map directly to the C++ interfaces:

### Trait: `IStatefulServiceReplica`

```rust
// User-implemented service logic
trait IStatefulServiceReplica {
    async fn open(&self, mode: OpenMode,
                  partition: &StatefulServicePartition,
                  cancellation_token: CancellationToken)
        -> Result<impl IPrimaryReplicator>;

    async fn change_role(&self, new_role: ReplicaRole,
                         cancellation_token: CancellationToken)
        -> Result<WString>;  // new listening address

    async fn close(&self, cancellation_token: CancellationToken)
        -> Result<()>;

    fn abort(&self);
}
```

### Trait: `IReplicator` (base)

```rust
// Replicator lifecycle — called by RA
trait IReplicator {
    async fn open(&self, cancellation_token: CancellationToken)
        -> Result<WString>;  // replicator address

    async fn change_role(&self, epoch: &Epoch, role: &ReplicaRole,
                         cancellation_token: CancellationToken)
        -> Result<()>;

    async fn update_epoch(&self, epoch: &Epoch,
                          cancellation_token: CancellationToken)
        -> Result<()>;  // called on secondaries only

    fn get_current_progress(&self) -> Result<i64>;   // end of log LSN
    fn get_catch_up_capability(&self) -> Result<i64>; // beginning of log LSN

    async fn close(&self, cancellation_token: CancellationToken)
        -> Result<()>;
    fn abort(&self);
}
```

### Trait: `IPrimaryReplicator` (extends `IReplicator`)

```rust
// Primary-side replication management — called by RA on primary
trait IPrimaryReplicator: IReplicator {
    async fn on_data_loss(&self, cancellation_token: CancellationToken)
        -> Result<u8>;  // 1 if state changed

    fn update_catch_up_replica_set_configuration(
        &self, current: &ReplicaSetConfig, previous: &ReplicaSetConfig
    ) -> Result<()>;

    fn update_current_replica_set_configuration(
        &self, current: &ReplicaSetConfig
    ) -> Result<()>;

    async fn wait_for_catch_up_quorum(
        &self, mode: ReplicaSetQuorumMode,
        cancellation_token: CancellationToken
    ) -> Result<()>;

    async fn build_replica(
        &self, replica: &ReplicaInformation,
        cancellation_token: CancellationToken
    ) -> Result<()>;

    fn remove_replica(&self, replica_id: i64) -> Result<()>;
}
```

**`remove_replica` semantics (from C++ source):**

`RemoveReplica` only operates on **idle replicas** — those in `InBuild` or `Idle`
state, NOT in the active configuration. Active secondaries are removed by
updating the configuration (removing them from the members list via
`update_catch_up_replica_set_configuration`).

When called on the primary's replicator (`ReplicaManager::RemoveReplica`):
1. Searches `idleReplicas_` list only
2. Closes the replication session
3. Calls `session->CancelCopy()` to cancel any in-progress copy/build
4. Updates progress and completes any now-committed replicate operations
5. Returns `REReplicaDoesNotExist` if the replica wasn't found

Used by the RA in two scenarios:
- **During re-build:** When a stale idle entry (older instance) exists for the same
  node. The old idle must be removed before the new build can start
  (`ReplicatorBuildIdleReplicaAsyncOperation`).
- **When `ReplicatorRemovePending` is set:** The RA marks a faulted/dropped replica
  for removal. On the next processing cycle, it sends `ReplicatorRemoveIdleReplica`
  to the primary's proxy.
```

### Key Types

```rust
struct Epoch {
    pub data_loss_number: i64,
    pub configuration_number: i64,
}

enum ReplicaRole {
    Unknown, None, IdleSecondary, ActiveSecondary, Primary,
    IdleAuxiliary, ActiveAuxiliary, PrimaryAuxiliary,
}

struct ReplicaInformation {
    pub id: i64,
    pub role: ReplicaRole,
    pub status: ReplicaStatus,      // Up or Down
    pub replicator_address: WString,
    pub current_progress: i64,      // LSN end of log
    pub catch_up_capability: i64,   // LSN beginning of log
    pub must_catch_up: bool,        // Primary candidate marker
}

struct ReplicaSetConfig {
    pub replicas: Vec<ReplicaInformation>,
    pub write_quorum: u32,
}

enum ReplicaSetQuorumMode {
    All,     // Full quorum catchup
    Write,   // Write quorum catchup (includes must_catch_up replica)
    Invalid,
}
```

---

## Comparison with CloudNativePG Failover

| Aspect | Service Fabric | CloudNativePG |
|---|---|---|
| **Failure detection** | Federation subsystem lease-based heartbeats + FM health monitoring | Operator polls HTTP `/pg/status` endpoint + Kubernetes liveness probes |
| **Primary selection** | FM's `CompareForPrimary()` — highest LSN among healthy replicas | Operator sorts replicas by ReceivedLsn → ReplayLsn → pod name |
| **Fencing mechanism** | Epoch-based: secondaries reject operations from stale epochs | Annotation-based: `cnpg.io/fencedInstances` + pod label updates for service routing |
| **Reconfiguration protocol** | 5-phase (Phase0–4) with explicit LSN queries, catchup, deactivation, activation | Simpler: signal pending → wait WAL receivers down → promote → pg_rewind |
| **Replication** | Built-in Fabric Replicator with quorum ACKs, copy/replication streams | Native PostgreSQL streaming replication + WAL archiving |
| **State management** | Reliable Collections (replicated key-value/queue) or custom state provider | PostgreSQL (full RDBMS with MVCC, WAL, etc.) |
| **Swap primary** | Double-catchup protocol with write-status revocation | Smart shutdown → WAL archive → pg_ctl promote |
| **Quorum model** | Write quorum = ⌊N/2⌋+1, dual-config quorum during transitions | Optional R+W>N quorum check (Dynamo-style) |
| **Data loss handling** | `on_data_loss()` callback, `data_loss_number` epoch increment | Not applicable at same level — relies on WAL archive for PITR |
| **Infrastructure** | No external dependencies — runs entirely within SF cluster | Kubernetes API as source of truth — no external consensus store |
| **Pod management** | Direct process management via Hosting subsystem | Direct Pod management (no StatefulSets) |

---

## Key Source Code References

### Service Fabric C++ (build/service-fabric)

| Component | Path | Description |
|---|---|---|
| **Failover Manager** | `src/prod/src/Reliability/Failover/fm/FailoverManager.h/cpp` | Primary election, failover decisions (~2966 lines) |
| **Reconfiguration Task** | `src/prod/src/Reliability/Failover/fm/ReconfigurationTask.h/cpp` | `CompareForPrimary()`, phase orchestration |
| **Reconfiguration Agent** | `src/prod/src/Reliability/Failover/ra/ReconfigurationAgent.h/cpp` | Node-local RA, DoReconfiguration handler (~4483 lines) |
| **Reconfiguration Phases** | `src/prod/src/Reliability/Failover/ra/FailoverUnitReconfigurationStage.h` | Phase0–Phase4 enum definitions |
| **Replicator** | `src/prod/src/Reliability/Replication/Replicator.h/cpp` | Replication engine (~986 lines) |
| **Primary Replicator** | `src/prod/src/Reliability/Replication/PrimaryReplicator.h` | Write path, quorum management |
| **Secondary Replicator** | `src/prod/src/Reliability/Replication/SecondaryReplicator.h` | Copy/replication stream receive |
| **Operation Stream** | `src/prod/src/Reliability/Replication/OperationStream.h` | Copy vs replication stream abstraction |
| **ReplicaRole** | `src/prod/src/Reliability/Failover/common/ReplicaRole.h` | Role enum |
| **Epoch** | `src/prod/src/ServiceModel/reliability/failover/Epoch.h` | Configuration versioning |
| **ReconfigurationType** | `src/prod/src/ServiceModel/reliability/failover/ReconfigurationType.h` | Failover vs SwapPrimary vs Other |
| **DoReconfiguration Message** | `src/prod/src/Reliability/Failover/common/DoReconfigurationMessageBody.h` | FM→RA protocol message |
| **FailoverUnit** | `src/prod/src/Reliability/Failover/fm/FailoverUnit.h/cpp` | `IsQuorumLost()`, `UpdateEpochForDataLoss()`, `ClearConfiguration()` |
| **FailoverUnit Config** | `src/prod/src/Reliability/Failover/fm/FailoverUnitConfiguration.h` | WriteQuorumSize, ReadQuorumSize formulas |
| **FailoverUnit Description** | `src/prod/src/Reliability/Failover/common/FailoverUnitDescription.h` | `IsDataLossBetweenPCAndCC`, epoch fields |
| **AccessStatus Enum** | `src/prod/src/Reliability/Failover/ra/AccessStatus.h` | TryAgain, NotPrimary, NoWriteQuorum, Granted |
| **ReadWriteStatusCalculator** | `src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.h/cpp` | Full state machine for read/write status per reconfiguration phase |
| **ReplicatorOnDataLoss** | `src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReplicatorOnDataLossAsyncOperation.cpp` | `IsDataLossBetweenPCAndCC` check, `BeginOnDataLoss()` invocation |
| **Replicator VerifyAccess** | `src/prod/src/Reliability/Replication/Replicator.cpp` | `VerifyAccessGranted()` — checks WriteStatus before replicate() |
| **ReplicaManager** | `src/prod/src/Reliability/Replication/ReplicaManager.cpp` | `HasEnoughReplicas()`, epoch storage, quorum tracking |
| **ReadWriteStatusValue** | `src/prod/src/Reliability/Failover/ra/ReadWriteStatusValue.h` | Pair of read + write AccessStatus |
| **ReadWriteStatusState** | `src/prod/src/Reliability/Failover/ra/ReadWriteStatusState.h` | State tracking on FailoverUnitProxy |
| **ComStatefulServicePartition** | `src/prod/src/Reliability/Failover/ra/ComStatefulServicePartition.h/cpp` | GetReadStatus/GetWriteStatus COM implementation |

### Rust Bindings (Azure/service-fabric-rs)

| File | Description |
|---|---|
| `crates/libs/core/src/runtime/stateful_traits.rs` | `IStatefulServiceReplica`, `IReplicator`, `IPrimaryReplicator` trait definitions with detailed doc comments |
| `crates/libs/core/src/runtime/stateful_types.rs` | Wrapper types |
| `crates/libs/core/src/runtime/stateful_proxy.rs` | COM proxy wrappers |
| `crates/libs/core/src/runtime/stateful_bridge.rs` | Rust↔COM bridge, parameter assertions |
| `crates/libs/core/src/types/runtime/stateful.rs` | `Epoch`, `ReplicaInformation`, `ReplicaSetConfig`, `ReplicaSetQuorumMode` |
| `crates/libs/core/src/types/common/stateful.rs` | `ReplicaRole` enum |

### Microsoft Documentation

| Page | Topic |
|---|---|
| [Replica Lifecycle](https://learn.microsoft.com/en-us/azure/service-fabric/service-fabric-concepts-replica-lifecycle) | Lifecycle states, role transitions |
| [Reconfiguration](https://learn.microsoft.com/en-us/azure/service-fabric/service-fabric-concepts-reconfiguration) | Reconfiguration types and phases |
| [Availability](https://learn.microsoft.com/en-us/azure/service-fabric/service-fabric-availability-services) | How SF maintains service availability |
| [Architecture](https://learn.microsoft.com/en-us/azure/service-fabric/service-fabric-architecture) | Subsystem stack overview |
| [Reliable Services Lifecycle](https://learn.microsoft.com/en-us/azure/service-fabric/service-fabric-reliable-services-lifecycle) | Startup, shutdown, primary swap sequences |
| [Reliable Collections](https://learn.microsoft.com/en-us/azure/service-fabric/service-fabric-reliable-services-reliable-collections) | Replicated state abstractions |
| [Partitioning](https://learn.microsoft.com/en-us/azure/service-fabric/service-fabric-concepts-partitioning) | Partition schemes and scaling |
