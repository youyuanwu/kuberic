# Service Fabric Stateful Service Failover Architecture

A detailed study of how Azure Service Fabric achieves high availability and
failover for stateful services — covering the system architecture, the
replicator protocol, reconfiguration phases, and the Rust API surface from
[service-fabric-rs](https://github.com/Azure/service-fabric-rs).

---

## Table of Contents

1. [System Architecture Overview](#system-architecture-overview)
2. [Key Concepts](#key-concepts)
3. [Replica Roles and Lifecycle](#replica-roles-and-lifecycle)
4. [The Replicator Protocol](#the-replicator-protocol)
5. [Failover: Reconfiguration in Detail](#failover-reconfiguration-in-detail)
6. [Switchover (Swap Primary)](#switchover-swap-primary)
7. [Epoch-Based Fencing](#epoch-based-fencing)
8. [Quorum and Consistency](#quorum-and-consistency)
9. [Building New Replicas](#building-new-replicas)
10. [Data Loss Handling](#data-loss-handling)
11. [Reliable Collections and the State Manager](#reliable-collections-and-the-state-manager)
12. [Rust API Surface (service-fabric-rs)](#rust-api-surface-service-fabric-rs)
13. [ReadStatus and WriteStatus](#readstatus-and-writestatus)
14. [Comparison with CloudNativePG Failover](#comparison-with-cloudnativepg-failover)
15. [Key Source Code References](#key-source-code-references)

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

## The Replicator Protocol

The **Replicator** (also called the Fabric Replicator when using the built-in
implementation) sits between the RA and the service's state provider. It
handles:

1. **Quorum-based replication** of operations from primary to secondaries
2. **Copy stream** for seeding new replicas with full state
3. **Replication stream** for incremental operations
4. **LSN tracking** for progress and catchup capability
5. **Configuration management** during reconfigurations

### Two Streams

| Stream | Direction | Purpose |
|---|---|---|
| **Copy Stream** | Primary → Idle Secondary | Full state transfer during `build_replica()`. Brings a new/empty replica up to the current committed state. |
| **Replication Stream** | Primary → Active Secondaries | Incremental operations. Every write on the primary is sent as an operation with a monotonically increasing LSN. |

### LSN (Log Sequence Number)

Every operation gets a unique, monotonically increasing LSN. Two LSN values
are tracked per replica:

| LSN | Meaning | API |
|---|---|---|
| **Current Progress** (end of log) | The highest LSN this replica has received/applied | `get_current_progress()` |
| **Catch-Up Capability** (beginning of log) | The oldest LSN this replica still has available | `get_catch_up_capability()` |

**Primary selection uses Current Progress** — the replica with the highest
current progress (most data) is the best candidate.

**Catch-up feasibility uses Catch-Up Capability** — if a secondary's current
progress is less than a candidate primary's catch-up capability, that secondary
cannot catch up incrementally and must be rebuilt from scratch.

### Quorum Acknowledgment

```
    Primary                Secondary-1            Secondary-2
       │                       │                       │
       │──── Op(LSN=42) ──────►│                       │
       │──── Op(LSN=42) ──────────────────────────────►│
       │                       │                       │
       │◄─── ACK(LSN=42) ─────│                       │
       │                       │                       │
       │   WriteQuorum met (2 of 3)                    │
       │   Commit LSN=42                               │
       │                       │                       │
       │◄─── ACK(LSN=42) ────────────────────────────│
       │                       │                       │
```

- **Write Quorum** = ⌊ReplicaCount / 2⌋ + 1 (majority including primary)
- An operation is **committed** once acknowledged by a write quorum
- During reconfiguration with both current and previous configurations,
  writes must be acknowledged by a write quorum of **both** configurations

---

## Failover: Reconfiguration in Detail

A **reconfiguration** is the process of moving from one configuration (Previous
Configuration, PC) to another (Current Configuration, CC). The Failover
Manager initiates reconfigurations in response to failures, load balancing, or
upgrades.

### Reconfiguration Types

| Type | Trigger | Primary Changes? |
|---|---|---|
| **Failover** | Unplanned primary failure | Yes — new primary elected |
| **SwapPrimary** | Load balancing, upgrade, user request | Yes — coordinated handoff |
| **Other** | Adding/removing secondaries | No — same primary |

### The Five-Phase Reconfiguration

**Source:** `src/prod/src/Reliability/Failover/ra/FailoverUnitReconfigurationStage.h`

```
┌─────────────────────────────────────────────────────────────────┐
│                    FAILOVER RECONFIGURATION                     │
│                   (Primary has failed)                          │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Phase 1: GET LSN                                               │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │ FM identifies correct new primary among current replicas  │  │
│  │ RA queries all secondaries for their current LSN          │  │
│  │ (get_current_progress)                                    │  │
│  │ Replica with highest LSN = best candidate                 │  │
│  └───────────────────────────────────────────────────────┬───┘  │
│                                                          │      │
│  Phase 2: CATCHUP                                        ▼      │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │ New primary waits for secondaries to catch up             │  │
│  │ Replicator sends missing operations to lagging replicas   │  │
│  │ wait_for_catch_up_quorum(All) blocks until all caught up  │  │
│  │ Ensures quorum has all committed data before proceeding   │  │
│  └───────────────────────────────────────────────────────┬───┘  │
│                                                          │      │
│  Phase 3: DEACTIVATE                                     ▼      │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │ Deactivate old configuration                              │  │
│  │ Prepare epoch change                                      │  │
│  │ State provider notified of deactivation                   │  │
│  └───────────────────────────────────────────────────────┬───┘  │
│                                                          │      │
│  Phase 4: ACTIVATE                                       ▼      │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │ New epoch applied (configuration_number incremented)      │  │
│  │ New primary activated with change_role(Primary)           │  │
│  │ Secondaries receive update_epoch() with new epoch         │  │
│  │ New primary begins accepting writes                       │  │
│  └───────────────────────────────────────────────────────────┘  │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

### Failover Message Flow

```
  Failover Manager (FM)              Reconfiguration Agent (RA)
         │                                      │
    1. Detects primary down                     │
    2. Evaluates replicas                       │
       (CompareForPrimary)                      │
    3. Selects new primary                      │
         │                                      │
         │──── DoReconfiguration ──────────────►│
         │     (new config, target primary,     │
         │      reconfiguration type=Failover)  │
         │                                      │
         │                              4. Phase 1: Query LSNs
         │                              5. Phase 2: Catchup
         │                              6. Phase 3: Deactivate
         │                              7. Phase 4: Activate
         │                                      │
         │◄──── ReplicaUp ────────────────────│
         │      (new primary confirmed)         │
         │                                      │
```

### Primary Selection Logic

The FM's `ReconfigurationTask::CompareForPrimary()` evaluates candidate
replicas based on:

1. **Data freshness** — highest LSN (current progress) wins
2. **Replica health** — must be Up, not StandBy or Down
3. **Quorum membership** — must be an Active Secondary in the current config
4. **Placement constraints** — ToBePromoted flags respected
5. **Configuration membership** — prefers replicas already in quorum

---

## Switchover (Swap Primary)

A **SwapPrimary** is a coordinated, planned primary change with zero data loss.
Used during application upgrades, load balancing, or manual intervention.

### Swap Primary Flow

```
┌─────────────────────────────────────────────────────────────────┐
│                    SWAP PRIMARY RECONFIGURATION                 │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  Phase 0: DEMOTE (unique to SwapPrimary)                        │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │ Current primary transfers state to new primary            │  │
│  │ Halt new replication writes                               │  │
│  │ Current primary transitions to Active Secondary           │  │
│  └───────────────────────────────────────────────────────┬───┘  │
│                                                          │      │
│  Double Catchup (enabled by default):                    ▼      │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │ 1st catchup: wait_for_catch_up_quorum(Write)              │  │
│  │    WITH write status still granted                        │  │
│  │    (allows replicator to make writes needed for catchup)  │  │
│  │                                                           │  │
│  │ 2. SF revokes write status on old primary                 │  │
│  │                                                           │  │
│  │ 2nd catchup: wait_for_catch_up_quorum(Write)              │  │
│  │    WITHOUT write status                                   │  │
│  │    (final sync — no new writes possible)                  │  │
│  └───────────────────────────────────────────────────────┬───┘  │
│                                                          │      │
│  Phase 4: ACTIVATE                                       ▼      │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │ New primary activated                                     │  │
│  │ New epoch applied                                         │  │
│  │ Old primary continues as Active Secondary                 │  │
│  └───────────────────────────────────────────────────────────┘  │
│                                                                 │
└─────────────────────────────────────────────────────────────────┘
```

**Key difference from failover:** Phase 0 exists only in SwapPrimary. The old
primary is alive and cooperates in the handoff. Phase 1 (Get LSN) is skipped
because the new primary is already chosen.

### Write Quorum Mode

`wait_for_catch_up_quorum` accepts a mode parameter:

| Mode | Semantics | Used When |
|---|---|---|
| `All` | Every replica in the configuration must catch up | Non-swap reconfigurations (secondary restarts) |
| `Write` | A write-quorum subset must catch up, and the subset MUST include the `must_catch_up` replica (the primary candidate) | Swap primary — avoids slow replicas blocking the swap |

---

## Epoch-Based Fencing

Epochs are the fundamental mechanism that prevents **split-brain** — stale
primaries cannot corrupt the new primary's state.

### How Fencing Works

1. Every reconfiguration increments the epoch's `configuration_number`
2. The new primary receives its epoch via `change_role(epoch, Primary)`
3. All secondaries receive the new epoch via `update_epoch(epoch)`
4. Secondaries **reject any operations from a primary with an older epoch**

```
Timeline:

   Epoch(0,1)                    Epoch(0,2)
   ─────────────────────────────┬──────────────────────
   Old Primary (P1)             │  New Primary (P2)
   sends Op(LSN=100)            │  sends Op(LSN=101)
                                │
   Secondary receives           │  Secondary receives
   Op from P1 with Epoch(0,1)   │  update_epoch(0,2)
                                │
                                │  Secondary now REJECTS
                                │  any ops with Epoch(0,1)
                                │  from old primary P1
```

### Data Loss Epoch

When write quorum is lost (e.g., majority of replicas fail simultaneously),
the `data_loss_number` is incremented. The new primary receives an
`on_data_loss()` callback, allowing the service to:
- Accept the loss and continue
- Restore from an external backup
- Signal that state has changed (triggers rebuild of all secondaries)

---

## Quorum and Consistency

### Write Quorum

```
Write Quorum = ⌊N/2⌋ + 1

Where N = total replicas in the configuration

Examples:
  3 replicas → Write Quorum = 2  (tolerates 1 failure)
  5 replicas → Write Quorum = 3  (tolerates 2 failures)
  7 replicas → Write Quorum = 4  (tolerates 3 failures)
```

### During Reconfiguration (Two Configurations)

When both a Previous Configuration (PC) and Current Configuration (CC) exist:

> At any time the replicator can have one or two configurations. There is
> always a current configuration which represents the set of replicas that are
> participating in replication along with the current write quorum. In addition
> there can be a previous configuration which represents the set of replicas
> that were in the previous configuration. When there is both a current and
> previous configuration the replicator must ensure that writes are
> acknowledged by a write quorum of **both** configurations.
>
> — `stateful_traits.rs`

This dual-quorum requirement ensures no committed data is lost during
configuration transitions.

### Consistency Guarantees

- **Strong consistency by default:** Transaction commits only return after the
  entire transaction is logged on a majority quorum (including the primary)
- **Linearizable reads on primary:** All reads on the primary see the latest
  committed state
- **Eventual consistency option:** Applications can acknowledge before async
  commit returns for lower latency

---

## Building New Replicas

When a new replica joins the partition (scaling out, replacing a failed node,
or rebalancing):

```
    Primary                              New Idle Secondary
       │                                        │
  1.   │◄──── SF creates idle replica ──────────│
       │                                        │
  2.   │──── build_replica(idle_info) ─────────►│
       │     [Copy Stream: full state transfer] │
       │     Op(copy,1), Op(copy,2), ...        │
       │     ...until current committed state   │
       │                                        │
  3.   │     SF promotes Idle → Active Secondary │
       │                                        │
  4.   │──── update_catch_up_config() ─────────►│
       │     [Include S in current config]      │
       │                                        │
  5.   │──── wait_for_catch_up_quorum() ───────►│
       │     [S catches up to committed LSN]    │
       │                                        │
  6.   │     SF grants ReadStatus to S          │
       │     Build complete                     │
       │                                        │
```

**Key constraints on `build_replica()` input:**
- Role must be `IdleSecondary`
- `current_progress` = -1 (unknown)
- `catch_up_capability` = -1 (unknown)
- `must_catch_up` = false
- Replica being built does NOT count towards quorum
- A replica cannot simultaneously be in-build and in the configuration

---

## Data Loss Handling

**Data loss** occurs when write quorum is lost — a majority of replicas
(including the primary) fail simultaneously, and the surviving replicas may
not have all committed operations.

### Sequence

1. FM detects quorum loss
2. FM increments `Epoch.data_loss_number`
3. FM selects the surviving replica with the highest LSN as new primary
4. New primary's `on_data_loss()` is called
5. If `on_data_loss()` returns `true` (state changed), SF rebuilds all other
   replicas from the new primary
6. If `false`, SF attempts incremental catchup

### Prevention

- Use `MinReplicaSetSize` ≥ 2 to require quorum for writes
- Place replicas across fault domains and upgrade domains
- Use synchronous replication (the default for Reliable Collections)
- Monitor health reports from `System.FM` for quorum warnings

---

## Reliable Collections and the State Manager

For services using the Reliable Services programming model, the **State
Manager** and **Reliable Collections** provide a high-level abstraction over
the replicator:

| Collection | Analogous To |
|---|---|
| **Reliable Dictionary** | `ConcurrentDictionary<K,V>` — replicated key-value store |
| **Reliable Queue** | `ConcurrentQueue<T>` — replicated FIFO queue |
| **Reliable Concurrent Queue** | Best-effort ordering for high throughput |

### How They Use the Replicator

```
   User Code                 State Manager              Replicator
      │                           │                         │
      │── tx.AddAsync(k,v) ──────►│                         │
      │                           │── Replicate(op) ───────►│
      │                           │                         │── Op ──► Secondaries
      │                           │                         │◄─ ACKs ─┘
      │                           │◄── Quorum committed ───│
      │◄── tx.CommitAsync() ─────│                         │
      │    returns                │                         │
```

- **All reads are local** (from the replica's own state)
- **All writes replicate** through the replicator before commit returns
- **Transactions** can span multiple Reliable Collections atomically
- **Persisted mode:** Data written to disk AND replicated (survives full
  cluster restart)
- **Volatile mode:** Data replicated but kept in-memory only (survives
  individual node failures, not full cluster loss)

### Primary Demotion and Write Revocation

When a primary is demoted:
1. Write access to Reliable Collections is **immediately revoked**
2. Any in-flight transactions get `FabricNotPrimaryException`
3. The service must handle this — transient exceptions can be retried on the
   new primary
4. Secondary replicas have **read-only access** to Reliable Collections

---

## ReadStatus and WriteStatus

Service Fabric exposes **ReadStatus** and **WriteStatus** on the stateful
service partition. These are the mechanism by which the runtime communicates to
user code whether it is safe to accept client requests. Services **must** check
these before processing reads or writes.

### PartitionAccessStatus Enum

| Value | Meaning | Service Action |
|---|---|---|
| **Granted** | The operation is allowed. The replica is primary and has quorum. | Process the request. |
| **ReconfigurationPending** (TryAgain) | A reconfiguration is in progress. The status will change when it completes. | Return a retryable error to the client. |
| **NotPrimary** | This replica is not the primary. It cannot accept reads or writes. | Redirect the client to the primary (via Naming Service). |
| **NoWriteQuorum** | The primary does not have a write quorum. Reads may still be allowed. | Return a retryable error for writes; reads may succeed. |

### How ReadStatus and WriteStatus Are Computed

**Source:** `src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp`

The RA computes ReadStatus and WriteStatus as a function of **two dimensions**:

1. **Lifecycle state** — Is the replica opening, ready, or closing?
2. **Reconfiguration state** — What phase of reconfiguration is in progress,
   and what type?

The calculator maintains separate state tables for four reconfiguration
scenarios, each with five sub-phases:

```
Reconfiguration Sub-Phases:
  PreWriteStatusCatchup   — Before the initial catchup (swap primary only)
  TransitioningRole       — Role change in progress
  CatchupInProgress       — Secondaries catching up
  CatchupCompleted        — Catchup done, deactivation/activation pending
  Completed               — Reconfiguration finished
```

### Status During Normal Operation (No Reconfiguration)

| Lifecycle State | ReadStatus | WriteStatus |
|---|---|---|
| **Opening Primary** | TryAgain | TryAgain |
| **Ready Primary** | Granted | Granted (if write quorum in CC) |
| **Other** (secondary, closing, closed) | NotPrimary | NotPrimary |

For a ready primary, WriteStatus is **dynamically** evaluated: it checks
`HasMinReplicaSetAndWriteQuorum()` in real time. If enough secondaries go
down that write quorum is lost, WriteStatus transitions to **NoWriteQuorum**
without any reconfiguration — the primary remains primary but cannot accept
writes until quorum is restored.

ReadStatus for a ready primary is unconditionally **Granted** — reads are
always served from local state.

### Status During Failover (Secondary Promoted to Primary)

Reconfiguration type: **PromoteToPrimary** (S→P, I→P, U→P)  
Phase order: Role Transition → Catchup → Deactivate → Activate

| Phase | ReadStatus | WriteStatus |
|---|---|---|
| TransitioningRole | TryAgain | TryAgain |
| CatchupInProgress | TryAgain | TryAgain |
| **CatchupCompleted** | **Granted** | TryAgain |
| **Completed** | Granted | **Granted** (if write quorum) |

**Key insight:** ReadStatus is granted **after catchup completes** (Phase 2
done) but WriteStatus is only granted **after the full reconfiguration
completes** (Phase 4 done). This means the new primary can serve reads before
it can accept writes, minimizing read unavailability.

### Status During Swap Primary (Primary Demoted to Secondary)

Reconfiguration type: **DemoteToSecondary** (P→S)  
Phase order: PreWriteStatusCatchup → Catchup → Role Transition → Complete

| Phase | ReadStatus | WriteStatus |
|---|---|---|
| **PreWriteStatusCatchup** | Granted | **Granted** (if quorum in PC AND CC) |
| CatchupInProgress | Granted | **TryAgain** (revoked) |
| CatchupCompleted | **TryAgain** (revoked) | TryAgain |
| TransitioningRole | NotPrimary | NotPrimary |
| Completed | NotPrimary | NotPrimary |

**Key insight:** During swap, the demoting primary goes through a carefully
staged revocation:

1. **PreWriteStatusCatchup** — Both read and write are still granted. The
   replicator performs an initial catchup while clients can still write. This
   is the "double catchup" optimization — catching up once with writes flowing
   reduces the final catchup window.
2. **WriteStatus revoked first** — Once the pre-catchup completes, write
   status is revoked (TryAgain) but reads continue.
3. **ReadStatus revoked after catchup** — Once the full catchup completes,
   reads are also revoked.
4. **NotPrimary after role transition** — Once the old primary becomes a
   secondary, both statuses become NotPrimary permanently.

This staged revocation minimizes the window where clients cannot read, and
ensures the new primary has all committed data before it starts accepting
writes.

### Status During No-Primary-Change Reconfiguration (P→P)

When secondaries are added/removed but the primary stays the same:

| Phase | ReadStatus | WriteStatus |
|---|---|---|
| CatchupInProgress | Granted | Granted (if quorum in PC AND CC) |
| CatchupCompleted | Granted | Granted (if quorum in PC AND CC) |
| Completed | Granted | Granted (if quorum in CC only) |

**Key insight:** Read and write remain granted throughout — there is no
disruption to client requests. The only change is that during the transition,
writes must be acknowledged by quorum from **both** the old and new
configurations (dual quorum). Once complete, only the new configuration's
quorum is needed.

### Status for Idle-to-Active Promotion (I→S)

| Phase | ReadStatus | WriteStatus |
|---|---|---|
| TransitioningRole | NotPrimary | NotPrimary |
| Completed | NotPrimary | NotPrimary |

Idle and active secondaries never get read or write access through this
mechanism (they are not primary).

### Dynamic Quorum Checks

When WriteStatus is marked as "dynamic" in the calculator, the actual value
is computed at query time:

```cpp
// From ReadWriteStatusCalculator.cpp
if (isDynamic_) {
    return fup.HasMinReplicaSetAndWriteQuorum(fupLock, needsPC_)
        ? AccessStatus::Granted
        : AccessStatus::NoWriteQuorum;
}
```

The `needsPC_` flag controls whether the check requires quorum from both
Previous Configuration and Current Configuration (during reconfiguration) or
just the Current Configuration (normal operation).

### How Services Should Use ReadStatus/WriteStatus

Services should check status **before** processing each request:

```csharp
// C# example pattern
public async Task<string> HandleReadRequest()
{
    // Check read access
    if (this.Partition.ReadStatus != PartitionAccessStatus.Granted)
        throw new FabricNotReadableException();

    // Proceed with read from Reliable Collections
    using (var tx = this.StateManager.CreateTransaction())
    {
        var value = await myDictionary.TryGetValueAsync(tx, key);
        return value.Value;
    }
}

public async Task HandleWriteRequest(string key, string value)
{
    // Check write access
    if (this.Partition.WriteStatus != PartitionAccessStatus.Granted)
        throw new FabricNotPrimaryException();

    // Proceed with write to Reliable Collections
    using (var tx = this.StateManager.CreateTransaction())
    {
        await myDictionary.SetAsync(tx, key, value);
        await tx.CommitAsync();  // Replicates to quorum
    }
}
```

**Important caveats:**
- The status can change **between the check and the actual operation**. The
  replicator may still throw `FabricNotPrimaryException` or similar even
  after a successful WriteStatus check.
- When a primary is demoted, write access to Reliable Collections is
  **immediately revoked** — in-flight transactions get `FabricNotPrimaryException`.
- Services must handle both `FabricNotPrimaryException` (permanent, redirect
  client) and `FabricTransientException` (transient, retry).

### ReadWriteStatusRevokedNotification

When read/write status is revoked during reconfiguration, the RA sends a
**ReadWriteStatusRevokedNotification** message to the failover unit. This
triggers the partition to update its cached status and unblock any threads
waiting on `GetReadStatus()` / `GetWriteStatus()` calls with the new value.

This notification mechanism ensures that long-running operations don't
continue operating under stale status — they are promptly informed of the
status change.

### Summary: Status Across the Full Failover Timeline

```
Time ──────────────────────────────────────────────────────────────►

OLD PRIMARY (P→S swap):
  Read:   Granted──────────────────┐TryAgain──┐NotPrimary──────────
  Write:  Granted──────┐TryAgain───┘           │
                       │                       │
           PreCatchup  Catchup     Completed   Role Changed
           (double     (write
            catchup)    revoked)

NEW PRIMARY (S→P failover):
  Read:   NotPrimary──┐TryAgain──────┐Granted──────────────────────
  Write:  NotPrimary──┘              │TryAgain──────┐Granted───────
                                     │              │
                      Role Changed   Catchup Done   Activate Done
```

### How the Replicator Reacts to ReadStatus/WriteStatus

The Fabric Replicator (the built-in replicator) checks WriteStatus **before
every replicate call**. This is the gate that prevents writes from being
accepted when the partition is in a transitional state.

**Source:** `src/prod/src/Reliability/Replication/Replicator.ReplicateAsyncOperation.cpp`

When `BeginReplicate()` is called by user code (via Reliable Collections or
directly), the replicator:

1. Acquires a read lock on its internal state
2. Calls `VerifyAccessGranted()` which reads `partition_.GetWriteStatus()`
3. Maps the status to an error code:

```
WriteStatus                        → Error Returned to Caller
─────────────────────────────────────────────────────────────
GRANTED                            → (proceed with replication)
RECONFIGURATION_PENDING (TryAgain) → ReconfigurationPending
NO_WRITE_QUORUM                    → NoWriteQuorum
NOT_PRIMARY                        → NotPrimary
```

4. If access is granted, verifies the replicator is actually in primary state
   (belt-and-suspenders check — if WriteStatus is Granted but the replicator
   is not primary, this is a test assertion failure)
5. Only then delegates to `PrimaryReplicator::BeginReplicate()` to enqueue the
   operation for quorum replication

**Key design:** The WriteStatus check happens **inside the replicator**, not
just in user code. This means even if user code forgets to check WriteStatus,
the replicator itself will reject the operation with the appropriate error.
User code checking first is an optimization to avoid the overhead of entering
the replicator's lock and constructing operation objects.

### How User Services Must React

User services interact with ReadStatus/WriteStatus at two levels: checking
the status proactively before operations, and handling exceptions from
operations that fail due to status changes.

#### Level 1: Proactive Status Checks

Before processing any client request, the service should check the
appropriate status on the partition object:

```
Client Request Type     Check                     On Non-Granted
──────────────────────────────────────────────────────────────────
Read                    Partition.ReadStatus       Return retryable error
                                                   or redirect to primary
Write                   Partition.WriteStatus      Return retryable error
                                                   or redirect to primary
Read-Write              Both ReadStatus AND        Handle each independently
                        WriteStatus
```

The check is cheap (reads a cached value on the partition object) and prevents
unnecessary work like deserializing request bodies, creating transactions, or
acquiring locks on Reliable Collections.

#### Level 2: Exception Handling from Reliable Collections / Replicator

Even after a successful status check, the status can change before the
operation completes. Services must handle these exceptions:

| Exception | Cause | Action |
|---|---|---|
| **FabricNotPrimaryException** | Replica lost primary role (WriteStatus became NotPrimary during operation) | **Permanent for this replica.** Close/abort any pending work. Client should resolve the new primary via Naming Service and retry there. |
| **FabricNotReadableException** | Replica lost read access (ReadStatus became NotPrimary or TryAgain) | **Permanent for this replica.** Client should resolve and retry. |
| **FabricTransientException** | Temporary condition (NoWriteQuorum, ReconfigurationPending) | **Transient.** Retry on the same replica after a brief delay. The condition may resolve when quorum is restored or reconfiguration completes. |
| **TimeoutException** | Operation waited too long for quorum acknowledgment | **Ambiguous.** The write may or may not have been committed. Client should re-read to check, then retry if needed. |
| **FabricObjectClosedException** | Partition/replica is shutting down | **Permanent.** Re-resolve the service and retry. |

#### Level 3: RunAsync Cancellation

When a primary is demoted, SF cancels the `CancellationToken` passed to
`RunAsync()`. Background work loops must honor this:

```
RunAsync(cancellationToken):
  while not cancellationToken.IsCancellationRequested:
    try:
      // Check write status before doing work
      if partition.WriteStatus != Granted:
        sleep(backoff)
        continue

      // Do work with Reliable Collections
      using tx = stateManager.CreateTransaction():
        await dictionary.SetAsync(tx, key, value)
        await tx.CommitAsync()

    catch FabricNotPrimaryException:
      // We lost primary, stop the loop
      return

    catch FabricTransientException:
      // Transient, retry after backoff
      sleep(backoff)
```

**Critical rule:** If `RunAsync()` does not exit promptly after cancellation,
the entire reconfiguration stalls. SF will eventually force-terminate the
service after a timeout (default 15 minutes), causing a health warning and
potentially a failed upgrade.

#### Level 4: ReportFault — Service-Initiated Failover

Services can also **proactively trigger failover** via `ReportFault()` on the
partition when they detect an unrecoverable condition:

| Fault Type | Effect |
|---|---|
| **Transient** | The replica is restarted on the same node. State is preserved (for persisted services). Equivalent to a process crash. |
| **Permanent** | The replica is dropped and rebuilt elsewhere. Local state is deleted. Use when the local state is corrupted. |

```
// Example: Service detects corrupted state
if stateIsCorrupted:
    partition.ReportFault(FaultType.Permanent)
    // SF will drop this replica and build a new one from another replica

// Example: Service detects transient issue (e.g., port conflict)
if needsRestart:
    partition.ReportFault(FaultType.Transient)
    // SF will restart this replica on the same node
```

`ReportFault(Permanent)` is particularly important for data integrity — it
tells SF that this replica's state cannot be trusted and must be rebuilt from
a healthy replica.

### Complete Request Processing Flow

Putting it all together, here is the full flow for a write request:

```
                Client
                  │
                  ▼
         ┌────────────────┐
         │ Service Replica │
         │ (Primary)       │
         └───────┬─────────┘
                 │
    ① Check Partition.WriteStatus
                 │
         ┌───────┴───────────────────┐
         │ Granted?                  │ No → return error to client
         │                           │      (NotPrimary / TryAgain /
         │                           │       NoWriteQuorum)
         ▼                           │
    ② Create Transaction             │
    ③ Perform writes on              │
       Reliable Collections          │
    ④ tx.CommitAsync()               │
         │                           │
         ▼                           │
  ┌──────────────────┐               │
  │ State Manager    │               │
  │ (Reliable Svcs)  │               │
  └──────┬───────────┘               │
         │                           │
         ▼                           │
  ┌──────────────────┐               │
  │ Replicator       │               │
  │                  │               │
  │ ⑤ VerifyAccess-  │               │
  │   Granted()      │               │
  │   (checks Write- │               │
  │    Status again)  │               │
  │                  │               │
  │ ⑥ If Granted:    │               │
  │   PrimaryRepl.   │               │
  │   BeginReplicate │               │
  │                  │               │
  │ ⑦ Send operation │               │
  │   to secondaries │               │
  │                  │               │
  │ ⑧ Wait for       │               │
  │   write quorum   │               │
  │   ACKs           │               │
  │                  │               │
  │ ⑨ Commit locally │               │
  └──────┬───────────┘               │
         │                           │
         ▼                           │
    Return success                   │
    to client                        │
```

**Key observation:** WriteStatus is checked **twice** — once by the service
(step ①) and once by the replicator (step ⑤). The service check is an
optimization; the replicator check is the authoritative gate. Between these
two checks, a reconfiguration could change the status, which is why exception
handling (Level 2 above) is essential even after a successful service-level
check.

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
| **FailoverUnit Config** | `src/prod/src/Reliability/Failover/fm/FailoverUnitConfiguration.h` | WriteQuorumSize, ReadQuorumSize |
| **AccessStatus Enum** | `src/prod/src/Reliability/Failover/ra/AccessStatus.h` | TryAgain, NotPrimary, NoWriteQuorum, Granted |
| **ReadWriteStatusCalculator** | `src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.h/cpp` | Full state machine for read/write status per reconfiguration phase |
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
