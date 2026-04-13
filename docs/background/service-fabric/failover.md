# Service Fabric: Failover and Recovery

Failover reconfiguration, switchover, epoch-based fencing,
failure detection, and quorum/data loss handling.

> Part of the [SF Architecture Reference](README.md).

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
| `All` | **Every** replica in the configuration must individually catch up to highest LSN | Legacy fallback for replicators that don't support `must_catch_up` |
| `Write` | A write-quorum subset must catch up, and the subset MUST include every `must_catch_up` replica | Default for modern replicators — swap, failover, restart |

**Source:** `FailoverUnitProxy.ReplicatorCatchupReplicaSetAsyncOperation.cpp`

The RA (Reconfiguration Agent) selects the mode via `GetCatchupModeCallerHoldsLock()`:

```cpp
case CatchupType::CatchupQuorum:
    return FABRIC_REPLICA_SET_WRITE_QUORUM;
case CatchupType::PreWriteStatusRevokeCatchup:
case CatchupType::CatchupDuringSwap:
    return replicator->DoesReplicatorSupportCatchupSpecificQuorum
        ? FABRIC_REPLICA_SET_WRITE_QUORUM
        : FABRIC_REPLICA_SET_QUORUM_ALL;
```

Three catchup types:
- **`CatchupQuorum`** — Standard reconfiguration (add/remove secondary).
  Always uses `Write` mode.
- **`PreWriteStatusRevokeCatchup`** — During switchover, before revoking
  write status on the old primary. Ensures the target secondary is caught up
  so no data is lost when writes stop. Uses `Write` (with `must_catch_up`)
  if supported, otherwise falls back to `All`.
- **`CatchupDuringSwap`** — During swap/switchover after demotion. Same
  fallback logic as pre-write-status revoke.

`FABRIC_REPLICA_SET_QUORUM_ALL` exists only as a **backward compatibility
fallback**. The `DoesReplicatorSupportCatchupSpecificQuorum` property was
added in a later SF version. Older replicators that can't mark specific
replicas as `must_catch_up` must wait for ALL replicas to catch up instead —
a stricter but correct alternative.

### SwapPrimary Action Sequence (from source)

The RA executes SwapPrimary as an ordered action list
(`ReconfigurationAgentProxy.ActionListExecutorAsyncOperation.cpp`):

```
1. ReplicatorPreWriteStatusRevokeUpdateConfiguration
   → UpdateConfiguration(PreWriteStatusRevokeCatchup)
   → Configure replicator for pre-revoke catchup

2. PreWriteStatusRevokeCatchup
   → BeginWaitForCatchUpQuorum(WRITE_QUORUM)
   → CATCHUP #1: target catches up BEFORE write revocation
   → Feature-flagged: IsPreWriteStatusCatchupEnabled (FailoverConfig)
   → When disabled: skips to step 3 (post-revoke catchup is the safety net)

3. ChangeReplicaRole
   → Old primary → ActiveSecondary (revokes write status)
   → After this: UpdateReadAndWriteStatus re-evaluates access

4. CatchupReplicaSetAll (CatchupDuringSwap)
   → BeginWaitForCatchUpQuorum(WRITE_QUORUM or QUORUM_ALL)
   → CATCHUP #2: final sync after write revocation
   → Guarantees target has all committed data

5. Phase4 Activate
   → New primary promoted with new epoch
```

**Key insight**: `PreWriteStatusRevokeCatchup` was added later as an
optimization (behind feature flag). The original SF design relied solely
on the post-revoke catchup (#2). The pre-revoke catchup (#1) reduces
the post-revoke catchup time by ensuring the target is already nearly
caught up before writes are stopped — but #2 alone is sufficient for
correctness.

### Double Catchup — Official Rust Trait Documentation

From the SF Rust bindings (`build/service-fabric-rs/.../stateful_traits.rs`,
`wait_for_catch_up_quorum` doc comment, lines 198-208):

> For swap primary case, double catchup feature is enabled by default.
> SF can first call this api before initiating write status revocation.
> SF then revoke write status, and call this again. This allows
> replicator to catch up with write status granted to make necessary
> writes for catch up. There is a chance that replicator takes forever
> to complete this api with mode `ReplicaSetQuarumMode::All` since
> client/user can keep writing and advancing the committed LSN, but
> it most likely would not stall in mode `ReplicaSetQuarumMode::Write`.

**Why `Write` mode is critical for the first catchup**: With `All` mode,
every replica must catch up. Since clients are still writing during the
first catchup, `committed_lsn` keeps advancing — a slow replica may
never converge. `Write` mode only requires a quorum subset including
the `must_catchup` replica (the swap target), so it converges even
under active writes.

The trait also documents the `must_catchup` semantics (lines 157-163):

> The total number of replicas marked with must_catchup will not exceed
> the write quorum. Secondary to be promoted to new primary is
> guaranteed to have must_catchup set, i.e. it must catch up (have all
> the data) to be promoted to new primary.

**Implication for kuberic**: Kuberic's proposed fix (one catchup before
demotion) matches SF's `PreWriteStatusRevokeCatchup`. Since kuberic's
`revoke_write_status` is an atomic flag (no writes can sneak in between
catchup and revoke), the post-revoke catchup (#2) is unnecessary —
the pre-revoke catchup alone guarantees the target has all data.

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

When the Failover Manager determines that data loss has occurred (see
[Quorum Loss and Data Loss](#quorum-loss-and-data-loss)), it increments
`Epoch.data_loss_number`. This is a distinct event from incrementing
`configuration_number`:

- **`configuration_number`** — incremented on every reconfiguration
  (failover, switchover, add/remove replica). Normal operation.
- **`data_loss_number`** — incremented only when FM explicitly declares
  data loss. Much rarer. Signals that committed operations may be lost.

**Source:** `FailoverUnit.cpp:867`

```cpp
void FailoverUnit::UpdateEpochForDataLoss()
{
    failoverUnitDesc_.CurrentConfigurationEpoch = Epoch(
        max(DateTime::Now().Ticks,
            failoverUnitDesc_.CurrentConfigurationEpoch.DataLossVersion + 1),
        failoverUnitDesc_.CurrentConfigurationEpoch.ConfigurationVersion);
}
```

Epoch comparison is lexicographic: `data_loss_number` is compared first,
then `configuration_number`. A data loss epoch always supersedes all
prior configuration-only epochs.

The new primary receives an `on_data_loss()` callback, allowing the
service to accept the loss, restore from backup, or signal state change
(which triggers rebuild of all secondaries). See
[Quorum Loss and Data Loss](#quorum-loss-and-data-loss) for the full
protocol.

---

## Failure Detection (from C++ source)

SF uses **layered failure detection** — it does NOT monitor replication
streams directly. Failure is detected at the node and process level:

### Layer 1: Federation Subsystem (node-level)

Nodes exchange **lease-based heartbeats** on a ring topology (128-bit
token space). When a node's lease expires, the Federation subsystem
declares `NodeDown`. This is fast (seconds) and catches entire node
failures — the most common failure mode.

### Layer 2: RA → FM (process-level)

When the RA (Reconfiguration Agent) on a node detects that a local
replica's process has crashed, it sends a `ReplicaDown` message to the
FM (Failover Manager):

**Source:** `FailoverManager.cpp:2340`
```cpp
void FailoverManager::ReplicaDownAsyncMessageHandler(Message & request, ...) {
    shared_ptr<ReplicaDownOperation> operation =
        make_shared<ReplicaDownOperation>(*this, from);
    operation->Start(operation, *this, move(body));
}
```

The FM then calls `FailoverUnit::OnReplicaDown()` which marks the replica
as `IsUp = false`, invalidates the configuration, and triggers
reconfiguration if the primary is affected.

### Layer 3: FM Health Monitoring (cluster-level)

The FM tracks replica health via periodic `ChangeNotification` messages
from each RA. If an RA stops sending notifications (because its node
died between heartbeat intervals), the FM eventually marks all replicas
on that node as down.

### What SF Does NOT Detect

A broken replication stream **without** a node or process failure (e.g.,
network partition between pods while both processes are alive) is NOT
directly detected by any layer. It manifests indirectly:
- The replicator stops receiving ACKs from the secondary
- If quorum is lost → writes fail with `NoWriteQuorum`
- The service can call `ReportFault()` to trigger failover
- Otherwise the partition operates in degraded mode silently

This is a known limitation of SF's design — the Federation lease system
catches most real-world failures (node death, network isolation from
cluster), but pod-to-pod network partitions within a healthy cluster
are harder to detect.

---


## Quorum Loss and Data Loss

These are **two distinct concepts** in Service Fabric. Quorum loss is a
runtime state where writes are blocked. Data loss is an explicit FM decision
that committed operations may be irrecoverable. The relationship is:
quorum loss *may* lead to data loss, but does not always.

### Definitions

| Concept | Definition | Triggered By |
|---------|-----------|--------------|
| **Quorum loss** | `UpCount < WriteQuorumSize` in the current configuration. Primary is alive but writes are blocked. | Automatic — detected when replicas fail |
| **Data loss** | FM increments `Epoch.data_loss_number`. Signals committed ops may be lost. | Explicit FM decision — only after timeout or total configuration loss |

### Quorum Loss

**Detection:** `FailoverUnit::IsQuorumLost()` checks two conditions:

```cpp
// FailoverUnit.cpp:1579
bool FailoverUnit::IsQuorumLost() const
{
    if (!HasPersistedState || CurrentConfiguration.IsEmpty)
        return false;

    // Write quorum loss: not enough UP replicas for writes
    if (CurrentConfiguration.UpCount < CurrentConfiguration.WriteQuorumSize)
        return true;

    // Read quorum loss during reconfiguration: can't guarantee
    // data in previous config is preserved
    return (IsInReconfiguration &&
            PreviousConfiguration.UpCount < PreviousConfiguration.ReadQuorumSize);
}
```

Where `WriteQuorumSize = ReplicaCount / 2 + 1` and
`ReadQuorumSize = (ReplicaCount + 1) / 2`.

**What happens during quorum loss:**

The primary is **alive** but the replicator **immediately blocks writes**.
`replicate()` does not hang or buffer — it returns `NoWriteQuorum` error
synchronously.

```cpp
// Replicator.cpp:889-908
bool Replicator::VerifyAccessGranted(__out ErrorCode & error)
{
    FABRIC_SERVICE_PARTITION_ACCESS_STATUS writeStatus =
        partition_.GetWriteStatus();
    switch(writeStatus)
    {
    case FABRIC_SERVICE_PARTITION_ACCESS_STATUS_NO_WRITE_QUORUM:
        error = ErrorCode(Common::ErrorCodeValue::NoWriteQuorum);
        return false;
    // ...
    }
}
```

The FM (not the replicator) controls the `WriteStatus`. The RA's
`ReadWriteStatusCalculator` dynamically checks:

```cpp
// FailoverUnitProxy.ReadWriteStatusCalculator.cpp
// WriteStatus is "dynamic" — computed on each query:
return fup.HasMinReplicaSetAndWriteQuorum(fupLock, needsPC_)
    ? AccessStatus::Granted
    : AccessStatus::NoWriteQuorum;
```

`HasMinReplicaSetAndWriteQuorum` requires BOTH:
1. `ccFailoverReplicaSetSize >= MinReplicaSetSize` — enough replicas exist
2. `ccReplicatorReplicaSetSize >= ccWriteQuorum` — enough are ACKing

**Source:** `FailoverUnitProxy.cpp:1399-1454`, `ReplicaManager.cpp:1351-1374`

### Quorum Loss Recovery (Without Data Loss)

When FM detects quorum loss, it does **not** immediately declare data loss.
Instead it waits:

```
Quorum Loss Timeline:

T0: Secondaries fail
    IsQuorumLost() → true
    FM records SetQuorumLossTime(now)
    FM fires FTQuorumLoss event
    Primary blocks writes (NoWriteQuorum)

T1: Waiting period (QuorumLossWaitDuration)
    FM checks on each background tick:
    - Are secondaries coming back?
    - Has the timeout expired?
    Primary stays alive, reads still work

T2a: Quorum RESTORED (secondaries recover)
    IsQuorumLost() → false
    FM fires FTQuorumRestored event
    Writes resume immediately
    NO DATA LOSS — no epoch change

T2b: Timeout exceeded (secondaries don't recover)
    FM calls DropOfflineReplicas()
    → Triggers data loss protocol (see below)
```

**Source:** `ReconfigurationTask.cpp:100-138`

```cpp
if (failoverUnit.IsQuorumLost())
{
    // Record when quorum loss started
    if (lockedFailoverUnit.Old && !lockedFailoverUnit.Old->IsQuorumLost())
    {
        failoverUnit.SetQuorumLossTime(DateTime::Now().Ticks);
    }

    TimeSpan quorumLossWaitDuration = (fm_->IsMaster ?
        FailoverConfig::GetConfig().FullRebuildWaitDuration :
        failoverUnit.ServiceInfoObj->ServiceDescription.QuorumLossWaitDuration);

    if (failoverUnit.LastQuorumLossDuration >= quorumLossWaitDuration)
    {
        lockedFailoverUnit.EnableUpdate();
        failoverUnit.DropOfflineReplicas();  // → data loss path
    }
}
else if (lockedFailoverUnit.Old && lockedFailoverUnit.Old->IsQuorumLost())
{
    // Quorum restored — NO data loss
    fm_->FTEvents.FTQuorumRestored(failoverUnit.Id.Guid, failoverUnit);
}
```

**Key insight:** `QuorumLossWaitDuration` is configurable per-service. The
default is generous — SF gives failed replicas time to recover before
declaring data loss. This is a critical design choice: the system
sacrifices write *availability* to preserve *consistency*.

### When Data Loss Is Declared

FM declares data loss by calling `UpdateEpochForDataLoss()` in two scenarios:

**Scenario 1: `ClearConfiguration()`** — All replicas are offline or
dropped (total loss). Called after `DropOfflineReplicas()` removes the
remaining offline replicas from the configuration.

```cpp
// FailoverUnit.cpp:1217-1222
void FailoverUnit::ClearConfiguration()
{
    UpdateEpochForDataLoss();  // Increment data_loss_number
    // Clear all config state...
}
```

**Scenario 2: `ClearCurrentConfiguration()`** — No replicas remain in the
current configuration and data previously existed:

```cpp
// FailoverUnit.cpp:1240-1246
void FailoverUnit::ClearCurrentConfiguration()
{
    bool dataloss = !NoData ||
        (static_cast<int>(CurrentConfiguration.ReplicaCount) >= MinReplicaSetSize);
    if (dataloss)
        UpdateEpochForDataLoss();
    // Clear current config...
}
```

The `dataloss` condition checks: if the service previously had data (`!NoData`)
OR the configuration was at least `MinReplicaSetSize`, then data loss is
real. If the service was still empty (`NoData == true`) and the config was
smaller than `MinReplicaSetSize`, no data loss is declared — there was
nothing to lose.

### The Data Loss Protocol

Once data loss is declared, the FM-RA-Replicator chain executes:

```
FM declares data loss:
  │
  ├─ UpdateEpochForDataLoss():
  │    new_epoch.data_loss_number = max(now.ticks, old + 1)
  │    (Uses wall-clock ticks in prod, simple +1 in test mode)
  │
  ├─ FM sends DoReconfiguration to RA:
  │    Contains: new epoch with higher data_loss_number
  │    IsDataLossBetweenPCAndCC = true
  │    (PC.DataLossVersion != CC.DataLossVersion)
  │
  ├─ RA receives on new primary:
  │    Checks IsDataLossBetweenPCAndCC
  │
  ├─ RA calls Replicator::BeginOnDataLoss():
  │    Replicator forwards to state provider's on_data_loss()
  │
  ├─ State provider's on_data_loss() returns isStateChanged:
  │
  │    ├─ TRUE: State provider had state at some LSN
  │    │    Replicator calls GetLastCommittedSequenceNumber()
  │    │    Replicator resets replication queue to that LSN
  │    │    FM rebuilds all secondaries from new primary
  │    │
  │    └─ FALSE: State provider discarded everything
  │         Cold start — no state recovery needed
  │         New primary starts fresh
  │
  └─ Primary transitions to Ready
     Writes resume with new epoch
```

**Source:** `FailoverUnitProxy.ReplicatorOnDataLossAsyncOperation.cpp:27-137`

```cpp
bool isDataLoss =
    owner_.ReplicaDescription.CurrentConfigurationRole == ReplicaRole::Primary &&
    owner_.FailoverUnitDescription.PreviousConfigurationEpoch != Epoch::InvalidEpoch() &&
    owner_.FailoverUnitDescription.IsDataLossBetweenPCAndCC;

if (!isDataLoss) {
    owner_.currentReplicaState_ = ReplicaStates::Ready;
    return;  // No callback needed
}

// Invoke state provider callback
AsyncOperationSPtr operation = owner_.replicator_->BeginOnDataLoss(...);
```

After the callback:

```cpp
if (isStateChanged_ == TRUE)
{
    error = parent_.stateProvider_.GetLastCommittedSequenceNumber(newProgress);
    parent_.stateProviderInitialProgress_ = newProgress;
    error = parent_.primary_->ResetReplicationQueue(
        parent_.stateProviderInitialProgress_);
}
```

### Data Loss vs Quorum Loss: Full Comparison

```
                    ┌──────────────────┐
                    │  Normal Operation │
                    │  Writes flowing   │
                    └────────┬─────────┘
                             │
                    Secondaries fail
                    (UpCount < WriteQuorumSize)
                             │
                    ┌────────▼─────────┐
                    │  QUORUM LOSS     │
                    │  Primary alive   │
                    │  Writes BLOCKED  │
                    │  (NoWriteQuorum) │
                    │  Reads still OK  │
                    └───┬──────────┬───┘
                        │          │
               Replicas │          │ QuorumLossWait-
               recover  │          │ Duration expires
                        │          │
              ┌─────────▼──┐   ┌───▼──────────────┐
              │  QUORUM    │   │  DropOffline-     │
              │  RESTORED  │   │  Replicas()       │
              │            │   │                   │
              │  No data   │   │  Config cleared   │
              │  loss      │   │  or reduced       │
              │  Writes    │   └───────┬───────────┘
              │  resume    │           │
              └────────────┘  UpdateEpochForDataLoss()
                                       │
                              ┌────────▼─────────┐
                              │  DATA LOSS       │
                              │  data_loss_number│
                              │  incremented     │
                              │                  │
                              │  FM sends        │
                              │  DoReconfigura-  │
                              │  tion with       │
                              │  IsDataLoss=true │
                              └────────┬─────────┘
                                       │
                              ┌────────▼─────────┐
                              │  on_data_loss()  │
                              │  callback on     │
                              │  new primary     │
                              │                  │
                              │  User decides:   │
                              │  accept/restore  │
                              │  /rebuild        │
                              └────────┬─────────┘
                                       │
                              ┌────────▼─────────┐
                              │  RECOVERY        │
                              │  Writes resume   │
                              │  Secondaries     │
                              │  rebuilt          │
                              └──────────────────┘
```

| Aspect | Quorum Loss | Data Loss |
|--------|------------|-----------|
| **What it is** | Runtime state: not enough replicas for writes | FM decision: committed ops may be irrecoverable |
| **Detection** | Automatic — `IsQuorumLost()` | Explicit FM decision after timeout |
| **Primary status** | Alive, blocks writes | May be alive or dead |
| **Reads** | Still work on primary | N/A during transition |
| **Writes** | `NoWriteQuorum` error immediately | Resume after `on_data_loss()` completes |
| **Epoch change** | No — same epoch | Yes — `data_loss_number` incremented |
| **User callback** | None | `on_data_loss()` on new primary |
| **Recovery** | Automatic when replicas return | Requires `on_data_loss()` + potential rebuild |
| **Can lead to other?** | Quorum loss → data loss (after timeout) | Data loss presupposes quorum loss |
| **Frequency** | Common (transient node failures) | Rare (catastrophic failures only) |
| **Configurable** | `WriteQuorumSize` (computed from replica count) | `QuorumLossWaitDuration` (per-service) |
| **MinReplicaSetSize** | Affects `HasMinReplicaSetAndWriteQuorum` check | Affects whether data loss is declared |

### Quorum Loss Without Data Loss — When It Happens

1. **Transient replica failure.** Secondary pods restart and rejoin before
   `QuorumLossWaitDuration` expires. Most common case.

2. **Network partition heals.** Secondaries were unreachable but come back
   with their state intact. Quorum restored, no data loss.

3. **Partial failure.** In a 5-replica set (quorum=3), 2 secondaries fail.
   Only 1 secondary left + primary = 2 < quorum=3. If one secondary
   recovers: 2 secondaries + primary = 3 = quorum. Writes resume.

### Data Loss Without Quorum Loss — Can It Happen?

**Not in normal operation.** Data loss requires `ClearConfiguration()` or
`ClearCurrentConfiguration()`, which are only called when replicas are
dropped (after quorum loss timeout). However, there is one edge case:

- During a reconfiguration, if the *read quorum* of the *previous*
  configuration is lost (`PreviousConfiguration.UpCount < ReadQuorumSize`),
  the FM may not be able to guarantee that all committed ops from the
  previous epoch were preserved. This is a subtle form of data loss that
  can occur even if the current configuration has write quorum.

### The Replicator's Role During Quorum Loss

The replicator is **passive** during quorum loss. It does not independently
detect or act on quorum loss:

1. **FM controls write status.** The RA's `ReadWriteStatusCalculator` sets
   `WriteStatus = NoWriteQuorum` dynamically. The replicator reads this via
   `partition_.GetWriteStatus()`.

2. **Replicator rejects writes.** `replicate()` calls `VerifyAccessGranted()`
   which checks `GetWriteStatus()`. If not `Granted`, returns error
   immediately — no buffering.

3. **FM controls data_loss_number.** The replicator never increments
   `data_loss_number` — it only stores and propagates the epoch that FM
   provides via `UpdateEpoch()`.

4. **Replicator executes on_data_loss.** When RA calls
   `Replicator::BeginOnDataLoss()`, the replicator forwards to the state
   provider's callback. If `isStateChanged == true`, the replicator resets
   its replication queue to the state provider's last committed LSN.

**Source:** `Replicator.cpp:889-908`, `ReplicaManager.cpp:881-923`,
`Replicator.OnDatalossAsyncOperation.cpp:82-137`

### MinReplicaSetSize vs WriteQuorumSize

These are related but distinct:

```
MinReplicaSetSize: The minimum number of replicas that must participate
                   in replication for writes to be accepted.

WriteQuorumSize:   ⌊ReplicaCount/2⌋ + 1. The majority needed for quorum.

Invariant:         MinReplicaSetSize ≤ WriteQuorumSize ≤ ReplicaCount
```

Both are checked by `HasMinReplicaSetAndWriteQuorum`:

```cpp
// FailoverUnitProxy.cpp:1399-1454
int ccWriteQuorum = ccFailoverReplicaSetSize / 2 + 1;
bool quorumCheck =
    ccFailoverReplicaSetSize >= serviceDescription_.MinReplicaSetSize &&
    ccReplicatorReplicaSetSize >= ccWriteQuorum;
```

Writes are blocked if **either** condition fails:
- Not enough replicas in the configuration (`< MinReplicaSetSize`)
- Not enough ACKing replicas for majority (`< WriteQuorumSize`)

The FM uses `MinReplicaSetSize` to decide whether to declare data loss
when clearing configuration: if the configuration had `≥ MinReplicaSetSize`
replicas, real data existed and loss is declared. If `< MinReplicaSetSize`,
the service may have been in a startup state with no committed data.

### Prevention

- Use `MinReplicaSetSize ≥ 2` to require quorum for writes
- Place replicas across fault domains and upgrade domains
- Use synchronous replication (the default for Reliable Collections)
- Monitor health reports from `System.FM` for quorum warnings
- Set `QuorumLossWaitDuration` appropriately (longer = more time for
  recovery, but longer write outage)

---

