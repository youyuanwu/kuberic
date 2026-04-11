# Service Fabric: Architecture and Concepts

System architecture, key concepts, and replica lifecycle for
Azure Service Fabric stateful services.

> Part of the [SF Architecture Reference](README.md).
> Source: [service-fabric-rs](https://github.com/Azure/service-fabric-rs)

---

## System Architecture Overview

Service Fabric is structured as a stack of layered subsystems:

```
┌──────────────────────────────────────────────────────────────┐
│                    Application Model                         │
│         Reliable Services · Reliable Actors · Guest          │
├──────────────────────────────────────────────────────────────┤
│                   Programming Models                         │
│       Stateful / Stateless / Actor frameworks                │
├──────────────────────────────────────────────────────────────┤
│                 Communication Subsystem                      │
│            Naming Service · Service Resolution               │
├──────────────────────────────────────────────────────────────┤
│                  Management Subsystem                        │
│      Cluster Manager · Health Manager · Image Store          │
├──────────────────────────────────────────────────────────────┤
│                  Testability Subsystem                       │
│             Fault Injection · Chaos Testing                  │
├──────────────────────────────────────────────────────────────┤
│                 Reliability Subsystem                        │
│    ┌──────────────┬──────────────────┬────────────────┐     │
│    │  Replicator   │  Failover Manager │  Resource      │     │
│    │  (quorum      │  (FM)             │  Balancer      │     │
│    │  replication)  │  (reconfiguration)│  (placement)   │     │
│    └──────────────┴──────────────────┴────────────────┘     │
├──────────────────────────────────────────────────────────────┤
│                  Hosting Subsystem                           │
│           Application Host · Code Package Lifecycle          │
├──────────────────────────────────────────────────────────────┤
│                  Federation Subsystem                        │
│      Ring topology · Failure detection · Leader election     │
│      (128-bit token space · lease-based heartbeats)          │
├──────────────────────────────────────────────────────────────┤
│                  Transport Subsystem                         │
│         Point-to-point datagrams · X.509 / Kerberos          │
└──────────────────────────────────────────────────────────────┘
```

The three components most relevant to stateful failover are:

| Component | Role |
|---|---|
| **Replicator** | Replicates state changes from primary to secondaries. Manages quorum acknowledgments. Provides copy and replication streams. |
| **Failover Manager (FM)** | Cluster-level service that detects failures, selects new primaries, and initiates reconfigurations. Runs as a system service inside the cluster. |
| **Reconfiguration Agent (RA)** | Node-local agent that executes reconfiguration phases on behalf of FM. Orchestrates the replicator and service replica lifecycle calls on each node. |

### How They Interact

```
         Cluster Level                          Node Level (per node)
    ┌─────────────────────┐              ┌──────────────────────────┐
    │   Failover Manager  │              │  Reconfiguration Agent   │
    │   (FM)              │──────────────│  (RA)                    │
    │                     │  DoReconfig  │                          │
    │  • Detects failures │  message     │  • Executes phases       │
    │  • Selects primary  │              │  • Calls replicator APIs │
    │  • Sends reconfig   │              │  • Manages replica       │
    │    decisions        │              │    lifecycle             │
    └─────────────────────┘              │                          │
                                         │  ┌────────────────────┐ │
                                         │  │ Replicator         │ │
                                         │  │ (Primary or        │ │
                                         │  │  Secondary)        │ │
                                         │  │                    │ │
                                         │  │ • Quorum ACKs      │ │
                                         │  │ • Copy/Repl streams│ │
                                         │  │ • LSN tracking     │ │
                                         │  └────────────────────┘ │
                                         │                          │
                                         │  ┌────────────────────┐ │
                                         │  │ Service Replica    │ │
                                         │  │ (user code)        │ │
                                         │  │                    │ │
                                         │  │ • Open/Close/Abort │ │
                                         │  │ • ChangeRole       │ │
                                         │  │ • RunAsync         │ │
                                         │  └────────────────────┘ │
                                         └──────────────────────────┘
```

---

## Key Concepts

### Partition

A **partition** is the unit of scalability. Each stateful service is divided
into one or more partitions (by range, name, or singleton). Each partition
maintains its own independent replica set. Failover happens at the partition
level — one partition's primary failing does not affect other partitions.

### Replica Set

Each partition has a **replica set** consisting of:
- Exactly **one Primary** — accepts reads and writes
- Zero or more **Active Secondaries** — receive replicated state, acknowledge
  writes, serve reads (if configured)
- Zero or more **Idle Secondaries** — being built/seeded, not yet participating
  in quorum

The **target replica set size** and **minimum replica set size** are configured
per service. For example, target=5, min=3 means the partition aims for 5
replicas and can tolerate 2 simultaneous failures.

### Configuration

A **configuration** is a snapshot of the replica set: which replicas exist and
what role each has. Configurations are versioned by **Epochs**.

### Epoch

An **Epoch** is a two-part version number:

```
Epoch {
    data_loss_number: i64,       // Incremented when write quorum is lost
    configuration_number: i64,   // Incremented on every reconfiguration
}
```

Epochs provide **total ordering** (data_loss_number compared first, then
configuration_number). They are the primary fencing mechanism — secondaries
reject operations from primaries with stale epochs.

---

## Replica Roles and Lifecycle

### Roles

| Role | Symbol | Description |
|---|---|---|
| **Primary** | P | Accepts reads and writes. Replicates operations to secondaries. |
| **Active Secondary** | S | Receives replication stream. Participates in quorum acknowledgment. Can serve reads. |
| **Idle Secondary** | I | Being built by the primary via copy stream. Not yet in quorum. |
| **None** | N | No role assigned (transitional state). |
| **Unknown** | U | Initial state before first `ChangeRole` call. |

### Role Transitions

```
U ──► P    Creation of a new primary (first replica in partition)
U ──► I    Creation of a new idle replica
I ──► S    Promotion: idle catches up, joins quorum as active secondary
I ──► P    Promotion: idle directly to primary (special reconfiguration)
S ──► P    Failover: active secondary promoted to primary
P ──► S    Demotion: primary demoted during swap/rebalance
S ──► N    Deletion of active secondary
P ──► N    Deletion of primary
I ──► N    Deletion of idle secondary
```

### Lifecycle States

Each replica goes through lifecycle states independent of its role:

```
                    ┌──────────┐
                    │ InBuild  │  Replica is being created/prepared
                    │  (IB)    │
                    └────┬─────┘
                         │
                         ▼
                    ┌──────────┐
                    │  Ready   │  Fully operational, participating in replication
                    │  (RD)    │
                    └────┬─────┘
                         │
              ┌──────────┼──────────┐
              ▼          ▼          ▼
        ┌──────────┐ ┌────────┐ ┌──────────┐
        │ Closing  │ │  Down  │ │ StandBy  │
        │  (CL)    │ │  (D)   │ │  (SB)    │
        └────┬─────┘ └───┬────┘ └────┬─────┘
             │           │           │
             ▼           ▼           ▼
        ┌──────────────────────────────────┐
        │           Dropped (DD)           │
        └──────────────────────────────────┘
```

- **Down (D):** Code stopped but persisted state remains on disk. Can be
  reopened without full rebuild.
- **StandBy (SB):** A previously-down replica reopened and kept warm. If the
  partition needs another replica, this one can rejoin faster because it
  already has partial state. Discarded after `StandByReplicaKeepDuration`.

### Service Replica Lifecycle API

The service replica (user code) implements four lifecycle methods:

```
open(mode, partition, token)  →  Returns the IPrimaryReplicator
change_role(new_role, token)  →  Returns new listening address
close(token)                  →  Graceful shutdown
abort()                       →  Ungraceful termination
```

**Ordering during startup:**
1. Service constructed
2. `open()` called — service returns its replicator
3. Communication listeners created and opened (primary only, unless
   `ListenOnSecondary=true`)
4. `RunAsync()` started (primary only) and `OnChangeRoleAsync()` called

**Ordering during primary demotion (swap):**
1. Listeners closed
2. `RunAsync()` cancellation token fired, wait for completion
3. Secondary-only listeners opened (if `ListenOnSecondary=true`)
4. `OnChangeRoleAsync()` called with new role

**Key detail:** The service object is NOT destroyed during a swap — only during
full shutdown. The same instance transitions between roles.

---

