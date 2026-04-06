# CloudNativePG Architecture

A detailed study of the [CloudNativePG](https://github.com/cloudnative-pg/cloudnative-pg)
(CNPG) Kubernetes operator — how it manages PostgreSQL clusters, orchestrates
failover, handles backups, and integrates with the broader Kubernetes ecosystem.

---

## Table of Contents

1. [Design Philosophy](#design-philosophy)
2. [High-Level Architecture](#high-level-architecture)
3. [Operator (Controller Manager)](#operator-controller-manager)
4. [Instance Manager](#instance-manager)
5. [Custom Resource Definitions](#custom-resource-definitions)
6. [Kubernetes Resources per Cluster](#kubernetes-resources-per-cluster)
7. [Replication Topology](#replication-topology)
8. [Failover Orchestration](#failover-orchestration)
9. [Switchover (Planned Failover)](#switchover-planned-failover)
10. [Split-Brain Prevention & Fencing](#split-brain-prevention--fencing)
11. [Backup & Recovery](#backup--recovery)
12. [WAL Archiving](#wal-archiving)
13. [Connection Pooling (Pooler)](#connection-pooling-pooler)
14. [Plugin System (CNPG-I)](#plugin-system-cnpg-i)
15. [Monitoring & Observability](#monitoring--observability)
16. [Storage Architecture](#storage-architecture)
17. [Networking & Service Routing](#networking--service-routing)
18. [Key Source Code Map](#key-source-code-map)

---

## Design Philosophy

CloudNativePG is built on several foundational decisions that distinguish it
from traditional PostgreSQL HA solutions (Patroni, Stolon, etc.):

| Principle | Description |
|---|---|
| **Kubernetes-native** | The Kubernetes API is the single source of truth. No external consensus store (etcd/ZooKeeper/Consul) beyond what Kubernetes already provides. |
| **No StatefulSets** | Pods and PVCs are managed directly by the operator, enabling LSN-based failover rather than ordinal-based, per-instance parameter tuning, and granular rolling updates. |
| **No management sidecars** | The Instance Manager runs as PID 1 inside the PostgreSQL container, keeping the attack surface minimal and avoiding sidecar coordination problems. |
| **Immutable infrastructure** | Containers are non-root, read-only-root-filesystem, with no SSH. Configuration changes are applied by replacing pods. |
| **Declarative** | All cluster topology, replication, backup, and pooling configuration lives in CRD specs. The operator continuously reconciles towards the declared state. |

---

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Kubernetes Cluster                          │
│                                                                 │
│  ┌─────────────────────┐                                        │
│  │  CNPG Operator Pod  │  (Deployment, typically 1 replica)     │
│  │  ┌───────────────┐  │                                        │
│  │  │  Controller    │  │  Watches: Cluster, Backup, Pooler,    │
│  │  │  Manager       │──┤  ScheduledBackup, Plugin CRDs         │
│  │  │  (cmd/manager) │  │  Reconciles: Pods, PVCs, Services,    │
│  │  └───────────────┘  │  Secrets, PDBs, Roles, etc.            │
│  └────────┬────────────┘                                        │
│           │ HTTP /pg/status                                     │
│           │ Kubernetes API                                      │
│           ▼                                                     │
│  ┌────────────────────────────────────────────────────────┐     │
│  │              PostgreSQL Cluster (3 Pods)                │     │
│  │                                                        │     │
│  │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐ │     │
│  │  │ Pod 1        │  │ Pod 2        │  │ Pod 3        │ │     │
│  │  │ (PRIMARY)    │  │ (REPLICA)    │  │ (REPLICA)    │ │     │
│  │  │              │  │              │  │              │ │     │
│  │  │ Instance Mgr │  │ Instance Mgr │  │ Instance Mgr │ │     │
│  │  │  (PID 1)     │  │  (PID 1)     │  │  (PID 1)     │ │     │
│  │  │      │       │  │      │       │  │      │       │ │     │
│  │  │  PostgreSQL   │  │  PostgreSQL   │  │  PostgreSQL   │ │     │
│  │  │  (child proc) │  │  (child proc) │  │  (child proc) │ │     │
│  │  │      │       │  │      │       │  │      │       │ │     │
│  │  │  ┌───┴───┐   │  │  ┌───┴───┐   │  │  ┌───┴───┐   │ │     │
│  │  │  │  PVC  │   │  │  │  PVC  │   │  │  │  PVC  │   │ │     │
│  │  │  └───────┘   │  │  └───────┘   │  │  └───────┘   │ │     │
│  │  └──────────────┘  └──────────────┘  └──────────────┘ │     │
│  └────────────────────────────────────────────────────────┘     │
│                                                                 │
│  Services:                                                      │
│    cluster-rw  ──► Primary only    (label: instanceRole=primary)│
│    cluster-ro  ──► Replicas only   (label: instanceRole=replica)│
│    cluster-r   ──► All ready pods                               │
│                                                                 │
│  Optional:                                                      │
│    ┌──────────────┐                                             │
│    │ Pooler       │  PgBouncer Deployment (separate pods)       │
│    │ (Deployment) │  Routes to -rw or -ro service               │
│    └──────────────┘                                             │
└─────────────────────────────────────────────────────────────────┘
```

---

## Operator (Controller Manager)

The operator is a standard Kubernetes controller-manager binary built with
[controller-runtime](https://github.com/kubernetes-sigs/controller-runtime).

**Entry point:** `cmd/manager/main.go`

The single binary serves multiple roles via subcommands:

```
manager controller          # Operator mode — runs reconciliation loops
manager instance run        # Instance Manager mode — runs inside PG pods
manager instance initdb     # Bootstrap a new cluster
manager instance join       # Join as a replica via pg_basebackup
manager instance restore    # Restore from backup
manager backup              # Execute a backup job
manager walarchive          # Archive WAL segments
manager walrestore          # Restore WAL segments
manager pgbouncer           # Run PgBouncer (Pooler pods)
```

### Controllers

The operator runs **five main controllers**, each reconciling a different CRD:

| Controller | CRD | File | Responsibility |
|---|---|---|---|
| **ClusterReconciler** | `Cluster` | `internal/controller/cluster_controller.go` (~1550 lines) | Core loop: manages Pods, PVCs, Services, Secrets, PDBs, replication, failover, rolling updates |
| **BackupReconciler** | `Backup` | `internal/controller/backup_controller.go` | Triggers and monitors backup jobs |
| **ScheduledBackupReconciler** | `ScheduledBackup` | `internal/controller/scheduledbackup_controller.go` | Creates Backup resources on cron schedule |
| **PoolerReconciler** | `Pooler` | `internal/controller/pooler_controller.go` | Manages PgBouncer Deployments and Services |
| **PluginReconciler** | `Plugin` | `internal/controller/plugin_controller.go` | Manages CNPG-I plugin lifecycle |

### Cluster Reconciliation Loop

The `ClusterReconciler.Reconcile()` method is the heart of the operator. On
each reconciliation it:

1. Fetches the `Cluster` resource and all owned Kubernetes resources
2. Queries every instance's HTTP status endpoint (`/pg/status`)
3. Detects the current primary and compares it to the target primary
4. Checks for split-brain (multiple primaries detected)
5. Reconciles infrastructure: Services, Secrets, PDBs, ConfigMaps
6. Reconciles instances: creates missing pods, deletes excess, rolls updates
7. Evaluates failover/switchover conditions
8. Updates `Cluster.Status` with optimistic locking (`pkg/resources/status/patch.go`)

### Status Management

Status updates use an **optimistic-locking patch pattern**:

1. Deep-copy current status
2. Apply transaction functions (modify status fields)
3. Compare semantic equality — skip patch if nothing changed
4. Patch via `MergeFromWithOptimisticLock`
5. Retry on etcd conflict (stale resourceVersion)

Key status fields on the `Cluster` resource:

| Field | Purpose |
|---|---|
| `currentPrimary` | Pod name of the active primary |
| `targetPrimary` | Pod name that *should* be primary (`"pending"` during failover) |
| `currentPrimaryTimestamp` | When the current primary was promoted |
| `currentPrimaryFailingSinceTimestamp` | When the primary started failing (for failover delay) |
| `phase` | Lifecycle state: `Healthy`, `Failing over`, `Switchover`, etc. |
| `readyInstances` | Count of healthy pods |
| `instancesStatus` | Per-pod status map (role, LSN, timeline, errors) |
| `topology` | Instance placement across nodes/zones |

---

## Instance Manager

The Instance Manager runs as **PID 1** inside every PostgreSQL pod. It is the
same binary as the operator, invoked with `manager instance run`.

**Key source:** `internal/management/controller/instance_controller.go` (~1430 lines)

### Responsibilities

| Area | Details |
|---|---|
| **PostgreSQL lifecycle** | Starts PostgreSQL as a child process, handles `SIGTERM` for graceful shutdown, manages `pg_ctl promote` on failover |
| **Health probes** | Runs an HTTP server that serves liveness, readiness, and startup probe endpoints |
| **Status reporting** | Periodically reports instance status (role, LSN, replication lag, WAL archiving state, timeline ID) via the `/pg/status` endpoint that the operator polls |
| **Configuration** | Applies `postgresql.conf` / `pg_hba.conf` changes, manages certificates and secrets |
| **WAL management** | Coordinates WAL archiving and restore via CNPG-I plugins |
| **pg_rewind** | After failover, automatically rewinds a demoted primary to rejoin the cluster as a replica |
| **Plugin host** | Discovers and manages CNPG-I plugin connections via Unix sockets |

### Health Probes

The instance manager exposes HTTP probes that Kubernetes uses:

**Startup probe** (default: up to 3600s):
- Checks PostgreSQL readiness via `pg_isready`, custom query, or streaming status
- With `streaming` mode: requires the replica to meet a `maximumLag` threshold
  before it is marked ready — prevents premature traffic during long recovery

**Liveness probe** (default: 30s timeout):
- On the primary: includes an **isolation check** (v1.27+) — if the primary
  cannot reach *both* the Kubernetes API and any peer instance, it considers
  itself isolated and triggers a restart to allow failover
- On replicas: always passes (replicas don't fence themselves)

**Readiness probe**:
- Primary: `pg_isready` success
- Replica: validates replication status and lag thresholds via `maximumLag`
- Drives which pods receive traffic through the `-rw`, `-ro`, `-r` Services

### Shutdown Sequence

1. **Smart shutdown** (`spec.smartShutdownTimeout`, default 180s) — disallow
   new connections, let existing queries finish, archive pending WAL
2. **Fast shutdown** — if smart timeout expires, forcibly terminate connections
3. **Switchover shutdown** (`spec.switchoverDelay`, default 3600s) — used
   during planned switchover, ensures WAL is fully archived before promotion

---

## Custom Resource Definitions

### Cluster

The primary CRD. Defines the entire PostgreSQL cluster topology.

```yaml
apiVersion: postgresql.cnpg.io/v1
kind: Cluster
metadata:
  name: my-cluster
spec:
  instances: 3                           # Total pods (1 primary + N-1 replicas)
  primaryUpdateStrategy: unsupervised    # or "supervised"
  failoverDelay: 0                       # Seconds to wait before failover
  switchoverDelay: 3600                  # Max seconds for graceful primary stop

  postgresql:
    parameters: { ... }                  # postgresql.conf overrides
    synchronous:
      method: any                        # "any" (quorum) or "first" (priority)
      number: 1                          # Sync replicas required
      dataDurability: required           # or "preferred"
      failoverQuorum: true               # Enable R+W>N quorum checks

  bootstrap:
    initdb: { ... }                      # New cluster
    recovery: { ... }                    # Restore from backup (PITR)
    pg_basebackup: { ... }              # Clone from external PG

  backup:
    target: prefer-standby               # Backup from replica when possible
    # Plugin-based or legacy barmanObjectStore

  storage:
    size: 10Gi
    storageClass: standard
  walStorage:                            # Optional separate WAL volume
    size: 2Gi

  monitoring:
    enablePodMonitor: false
    customQueriesConfigMap: [...]

  managed:
    services: { ... }                    # Service template customization
```

### Backup

Represents a single backup operation.

```yaml
apiVersion: postgresql.cnpg.io/v1
kind: Backup
spec:
  cluster:
    name: my-cluster
  method: barmanObjectStore | volumeSnapshot | plugin
  target: prefer-standby | primary
  online: true                           # Hot backup (no downtime)
```

**Phases:** `pending` → `started` → `running` → `finalizing` → `completed` (or `failed`)

### ScheduledBackup

Cron-driven backup creation.

```yaml
apiVersion: postgresql.cnpg.io/v1
kind: ScheduledBackup
spec:
  schedule: "0 0 0 * * *"               # 6-field cron (with seconds)
  cluster:
    name: my-cluster
  immediate: true                        # Run one immediately on creation
  backupOwnerReference: self             # none | self | cluster
```

### Pooler

PgBouncer-based connection pooling.

```yaml
apiVersion: postgresql.cnpg.io/v1
kind: Pooler
spec:
  cluster:
    name: my-cluster
  type: rw                               # rw | ro | r
  instances: 2
  pgbouncer:
    poolMode: transaction                # session | transaction
    parameters: { ... }
```

### FailoverQuorum

Auto-managed by the operator when `failoverQuorum: true`. Tracks synchronous
replication metadata needed for quorum-based failover decisions.

```go
type FailoverQuorumStatus struct {
    Method        string   // "ANY" or "ALL"
    StandbyNames  []string // Potentially synchronous instance names
    StandbyNumber int      // Required sync replicas (W in R+W>N)
    Primary       string   // Primary that last updated this object
}
```

---

## Kubernetes Resources per Cluster

For a 3-instance cluster named `example`, the operator creates and manages:

| Resource Type | Count | Names / Purpose |
|---|---|---|
| **Pod** | 3 | `example-1`, `example-2`, `example-3` — each runs Instance Manager + PostgreSQL |
| **PVC** | 3+ | One per pod for PGDATA, optionally separate WAL PVCs |
| **Service** | 3 | `example-rw` (primary), `example-ro` (replicas), `example-r` (all ready) |
| **Secret** | 4 | CA certificate, server TLS, replication credentials, application credentials |
| **PodDisruptionBudget** | 2 | Primary PDB (minAvailable=1), Replica PDB (minAvailable=N-2) |
| **ConfigMap** | 1+ | PostgreSQL configuration, custom monitoring queries |
| **ServiceAccount** | 1 | For in-pod Kubernetes API access |
| **Role + RoleBinding** | 1+1 | RBAC for the ServiceAccount |

Note: **No StatefulSet** — the operator manages Pods and PVCs directly,
enabling per-instance control, LSN-based failover, and coordinated parameter
updates across the cluster.

---

## Replication Topology

CNPG uses **native PostgreSQL streaming replication** (physical replication).

### Streaming Replication

- Primary streams WAL records to replicas in real-time
- Each replica connects to the primary using the `streaming_replica` user
- Replication slots prevent the primary from recycling WAL files that replicas
  still need

### Synchronous Replication

Configured via `spec.postgresql.synchronous`:

| Setting | Description |
|---|---|
| `method: any` | Quorum-based: transaction commits after *any* N replicas acknowledge |
| `method: first` | Priority-based: transaction commits after the *first* N replicas (in order) acknowledge |
| `number: N` | How many replicas must acknowledge before commit returns |
| `dataDurability: required` | Synchronous mode is mandatory — blocks if replicas unavailable |
| `dataDurability: preferred` | Falls back to asynchronous if sync replicas unavailable |

The operator dynamically manages the `synchronous_standby_names` PostgreSQL
parameter by watching pod status. When a sync replica goes down, the operator
updates the parameter to reflect the new set of available standbys.

### Sensitive Parameters

Parameters like `max_connections`, `max_wal_senders`, `max_prepared_transactions`
must be **equal or greater** on standbys compared to the primary. The operator
coordinates these changes in the correct order:

1. Update standbys first (increase values)
2. Then update primary
3. Reverse order for decreases

---

## Failover Orchestration

This is the most critical part of the operator. Automatic failover handles
unplanned primary failure.

**Core source:** `internal/controller/replicas.go` (~450 lines)  
**Entry point:** `reconcileTargetPrimaryFromPods()`

### Step-by-Step Failover Sequence

```
                    Primary Fails
                         │
                         ▼
            ┌────────────────────────┐
            │ 1. DETECTION           │
            │                        │
            │ Operator polls         │
            │ /pg/status on all pods │
            │ Primary returns error  │
            │ or is unreachable      │
            └───────────┬────────────┘
                        │
                        ▼
            ┌────────────────────────┐
            │ 2. FAILOVER DELAY      │
            │                        │
            │ If spec.failoverDelay  │
            │ > 0, wait N seconds    │
            │ Records timestamp in   │
            │ CurrentPrimaryFailing-  │
            │ SinceTimestamp          │
            │ (prevents flapping)    │
            └───────────┬────────────┘
                        │
                        ▼
            ┌────────────────────────┐
            │ 3. QUORUM CHECK        │
            │ (if enabled)           │
            │                        │
            │ Evaluate R + W > N     │
            │ R = ready sync replicas│
            │ W = required sync acks │
            │ N = total sync set     │
            │                        │
            │ If unsafe → BLOCK      │
            │ failover, requeue      │
            └───────────┬────────────┘
                        │
                        ▼
            ┌────────────────────────┐
            │ 4. SIGNAL OLD PRIMARY  │
            │                        │
            │ Set TargetPrimary =    │
            │ "pending"              │
            │ Set Phase = "Failing   │
            │ over"                  │
            │                        │
            │ Old primary's Instance │
            │ Manager sees this and  │
            │ initiates shutdown     │
            └───────────┬────────────┘
                        │
                        ▼
            ┌────────────────────────┐
            │ 5. WAIT FOR WAL        │
            │ RECEIVERS TO STOP      │
            │                        │
            │ AreWalReceiversDown()  │
            │ ensures no replica is  │
            │ still streaming from   │
            │ the old primary        │
            │                        │
            │ Prevents data          │
            │ divergence             │
            └───────────┬────────────┘
                        │
                        ▼
            ┌────────────────────────┐
            │ 6. SELECT BEST REPLICA │
            │                        │
            │ Sort replicas by:      │
            │  a. Error status       │
            │  b. ReceivedLsn (desc) │
            │  c. ReplayLsn (desc)   │
            │  d. Pod name (asc)     │
            │                        │
            │ Winner = Items[0]      │
            └───────────┬────────────┘
                        │
                        ▼
            ┌────────────────────────┐
            │ 7. PROMOTE             │
            │                        │
            │ Set TargetPrimary =    │
            │ winning replica name   │
            │                        │
            │ Instance Manager in    │
            │ that pod executes:     │
            │ pg_ctl promote         │
            │                        │
            │ Update pod labels:     │
            │ instanceRole=primary   │
            └───────────┬────────────┘
                        │
                        ▼
            ┌────────────────────────┐
            │ 8. OLD PRIMARY REJOIN  │
            │                        │
            │ When old primary pod   │
            │ restarts, Instance Mgr │
            │ detects it's no longer │
            │ target primary and     │
            │ runs pg_rewind:        │
            │                        │
            │ pg_rewind              │
            │   --source-server=     │
            │     <new primary>      │
            │   --target-pgdata=     │
            │     <local PGDATA>     │
            │   --restore-target-wal │
            │                        │
            │ Then starts as replica │
            └────────────────────────┘
```

### Replica Ranking Algorithm

The sort function in `pkg/postgres/status.go` (`PostgresqlStatusList.Less()`)
determines which replica gets promoted:

1. **Pods with errors** → sorted to the bottom (never promoted)
2. **Primary instances** → sorted first (for status display, not promotion)
3. **ReceivedLsn** (descending) → replica with the most WAL data received wins
4. **ReplayLsn** (descending) → tiebreaker: most WAL applied
5. **Pod name** (ascending) → final tiebreaker: alphabetical

This LSN-based ranking is the key advantage of not using StatefulSets — the
operator promotes the replica with the *least data loss*, not the one with the
lowest ordinal.

### Failover Delay

Configured via `spec.failoverDelay` (default: 0, meaning immediate).

When set to a positive value, the operator records
`CurrentPrimaryFailingSinceTimestamp` on first detection and waits the
configured number of seconds before proceeding. This prevents failover
oscillation during transient network partitions.

During online upgrades, a **minimum 30-second delay** is enforced regardless
of the configured value.

### Quorum-Based Failover

When `spec.postgresql.synchronous.failoverQuorum: true`, the operator applies
a **Dynamo-style R+W>N consistency check** before allowing failover:

```
R = number of ready replicas in the synchronous standby set
W = required synchronous acknowledgments (synchronous_standby_names)
N = total replicas in the synchronous standby set

Failover is ALLOWED only if: R + W > N
```

**Example:** 3-node cluster, `sync.number=1` (W=1), N=2 sync standbys:
- Primary fails, both replicas up: R=2, 2+1=3 > 2 ✓ → failover proceeds
- Primary + 1 replica fail: R=1, 1+1=2 = 2 ✗ → failover **blocked**

This guarantees that at least one promotable replica holds all committed
transactions. The `FailoverQuorum` CRD tracks the synchronous replication
state; the primary's Instance Manager updates it, and the operator resets it
when the sync configuration changes.

---

## Switchover (Planned Failover)

A switchover is a **graceful, user-initiated** primary change with zero data
loss.

**Source:** `internal/controller/cluster_upgrade.go`

### Primary Update Strategies

| Strategy | Behavior |
|---|---|
| `unsupervised` | Operator automatically triggers switchover during rolling updates |
| `supervised` | Operator pauses and sets phase to `Waiting for user`. User must explicitly request switchover (via `kubectl cnpg promote` or annotation) |

### Switchover Flow

1. User triggers switchover (or operator decides during rolling update)
2. Operator verifies the current primary is not fenced and is reachable
3. Operator sets `TargetPrimary` to the chosen replica
4. Primary's Instance Manager detects the change and initiates **smart
   shutdown**: stops accepting new connections, drains in-flight queries,
   archives remaining WAL
5. If smart shutdown exceeds `switchoverDelay` (default 3600s), falls back to
   fast shutdown
6. Target replica's Instance Manager executes `pg_ctl promote`
7. Old primary restarts as a replica (via pg_rewind if needed)
8. Operator updates pod labels → Services re-route traffic

---

## Split-Brain Prevention & Fencing

### Multiple-Primary Detection

The operator checks for split-brain on every reconciliation:

```go
if primaryNames := instancesStatus.PrimaryNames(); len(primaryNames) > 1 {
    // Pause all reconciliation, requeue after 5 seconds
    // Wait for old primary to acknowledge demotion
}
```

If multiple pods report as primary, the operator **halts all actions** and
waits for the situation to resolve naturally (old primary recognizes demotion).

### Fencing

Fencing isolates instances to prevent them from serving traffic during
ambiguous states.

**Mechanism:** A JSON annotation on the `Cluster` resource:

```yaml
metadata:
  annotations:
    cnpg.io/fencedInstances: '["cluster-example-1"]'   # Specific instance
    # or
    cnpg.io/fencedInstances: '["*"]'                    # All instances
```

- Fenced instances cannot be promoted to primary
- Fenced instances are excluded from service endpoints
- Switchover is blocked if the current primary is fenced
- The `kubectl cnpg` plugin provides commands to fence/unfence instances

### Primary Isolation Check (v1.27+)

The primary's liveness probe includes an **isolation check** that detects
network partitions from the primary's own perspective. This is a critical
defense-in-depth mechanism against split-brain: even if the operator cannot
reach the primary to demote it, the primary will **self-fence** by failing
its liveness probe, causing Kubernetes to restart the pod.

**Source:** `pkg/management/postgres/webserver/probes/liveness.go`,
`pkg/management/postgres/webserver/probes/pinger.go`

#### How It Works

The liveness probe on the primary executes this decision tree on every check:

```
Liveness probe fires on PRIMARY:
  │
  ├─ Can reach Kubernetes API server?
  │    YES → Liveness passes (OK)
  │           (also runs peer check as a warning-only diagnostic)
  │
  │    NO → API server unreachable
  │         │
  │         ├─ Have we ever received a Cluster definition?
  │         │    NO → Pass (OK) — too early to judge, defer decision
  │         │
  │         │    YES → Check peer reachability
  │         │         │
  │         │         ├─ Can reach ANY other instance?
  │         │         │    YES → Pass (OK) — not fully isolated
  │         │         │
  │         │         │    NO → FAIL (HTTP 500)
  │         │         │         Primary is isolated from BOTH
  │         │         │         API server AND all peers
  │         │         │         → Kubelet restarts the pod
  │         │         │         → Operator promotes a reachable replica
```

**Key rule:** The liveness probe fails only when **both** conditions are true:
1. The instance manager cannot reach the Kubernetes API server
2. The instance manager cannot reach **any** other instance via HTTPS

This dual-condition prevents false positives: a primary that can still reach
its peers (just not the API server) continues operating normally. Only true
network isolation triggers the self-fence.

#### Peer Reachability Check (Pinger)

The pinger component (`pinger.go`) checks instance reachability by calling
each peer's HTTPS `/failsafe` endpoint:

- Iterates over `cluster.Status.InstancesReportedState` (all known instances
  and their IPs, from the last successful API server fetch)
- For each peer: makes an HTTPS GET to `https://<peer-ip>:<status-port>/failsafe`
- Uses the PostgreSQL server CA certificate for TLS verification (avoids
  depending on the API server for certificate retrieval)
- Configurable timeouts: `connectionTimeout` (TCP dial) and `requestTimeout`
  (HTTP round-trip), both default 1000ms

If **any** peer responds, the primary is not isolated.

#### Configuration

```yaml
spec:
  probes:
    liveness:
      isolationCheck:
        enabled: true          # Default: true (since v1.27)
        requestTimeout: 1000   # Milliseconds, default 1000
        connectionTimeout: 1000 # Milliseconds, default 1000
```

**Special cases:**
- **Disabled** (`enabled: false`): Primary never self-fences. Useful in
  environments where the API server is frequently unreachable (e.g., edge
  deployments).
- **Single-instance clusters** (`spec.instances: 1`): Isolation check is
  skipped — there are no peers to check, so the primary can never be
  classified as isolated.
- **Replicas only:** Isolation check only runs on primaries. Replicas always
  pass the liveness check — there's no benefit to restarting an isolated
  replica.

#### Why This Matters for Split-Brain Prevention

Without the isolation check, a network-partitioned primary continues
accepting writes indefinitely — it doesn't know it's been replaced. Clients
routed to the old primary (via stale DNS, cached connections, etc.) create a
split-brain. The isolation check ensures the old primary **stops itself**
within `failureThreshold × periodSeconds` of losing connectivity (default:
~30 seconds), giving the operator a clean window to promote a new primary.

### Pod Disruption Budgets

The operator creates two PDBs per cluster:

| PDB | Selector | MinAvailable | Purpose |
|---|---|---|---|
| Primary PDB | `cnpg.io/podRole=primary` | 1 | Prevents voluntary eviction of the primary |
| Replica PDB | cluster instances | N-2 | Allows draining one replica at a time |

---

## Backup & Recovery

### Backup Methods

| Method | How it works | WAL Archive Required | Incremental | PITR Support |
|---|---|---|---|---|
| **Object Store** (Barman Cloud) | `pg_basebackup` to S3/Azure/GCS via Barman Cloud plugin | Yes | No | Yes |
| **Volume Snapshot** | Kubernetes CSI VolumeSnapshot of PVC(s) | Recommended | CSI-dependent | With WAL archive |
| **Plugin** | Custom CNPG-I plugin | Configurable | Plugin-dependent | Plugin-dependent |

### Backup Flow

1. User creates a `Backup` resource (or `ScheduledBackup` triggers one)
2. Operator sets backup phase to `pending`
3. Operator selects target pod (`prefer-standby` uses most-synchronized
   replica, falls back to primary)
4. Backup job executes: `started` → `running` → `finalizing` → `completed`
5. Status records: begin/end WAL, LSN positions, backup ID, destination path

### Point-in-Time Recovery (PITR)

Restoring a cluster to a specific moment requires:
- A base backup (object store or volume snapshot)
- Continuous WAL archive up to the recovery target

```yaml
spec:
  bootstrap:
    recovery:
      source: backup-cluster
      recoveryTarget:
        targetTime: "2024-01-15 10:30:00"    # Specific timestamp
        # or targetLSN, targetXID, targetImmediate
```

### Bootstrap Methods

| Method | Use Case |
|---|---|
| `initdb` | Create a brand-new cluster |
| `recovery` | Restore from backup (PITR capable) |
| `pg_basebackup` | Clone from an external running PostgreSQL |

---

## WAL Archiving

WAL (Write-Ahead Log) archiving is essential for PITR and for ensuring
durability beyond what streaming replication provides.

### Architecture

- **Plugin-based:** WAL archiving is delegated to CNPG-I plugins (the primary
  plugin is the Barman Cloud Plugin maintained by the CloudNativePG community)
- **One archiver per cluster:** Only one plugin can be designated as
  `isWALArchiver: true`
- **Default archive_timeout:** 5 minutes (configurable) — maximum time before
  a partially-filled WAL segment is force-archived
- **Object store support:** Amazon S3, Azure Blob Storage, Google Cloud
  Storage (via Barman Cloud)

### WAL Status Tracking

The Instance Manager tracks WAL archiving health and reports it to the
operator:

| Field | Description |
|---|---|
| `LastArchivedWAL` | Name of the last successfully archived WAL file |
| `LastArchivedWALTime` | Timestamp of last successful archive |
| `LastFailedWAL` | Name of the last WAL that failed to archive |
| `ReadyWALFiles` | Count of `.ready` files waiting to be archived |
| `CurrentWAL` | WAL segment currently being written |

### Shutdown & WAL Safety

During shutdown (both failover and switchover), the Instance Manager
prioritizes WAL archiving:

1. Smart shutdown: stop new connections, archive pending WAL
2. Wait up to `switchoverDelay` seconds for archiving to complete
3. Only then proceed with PostgreSQL shutdown
4. If disk space is exhausted, the Instance Manager detects it proactively to
   prevent corruption

---

## Connection Pooling (Pooler)

The `Pooler` CRD manages a **PgBouncer** deployment that sits between
application clients and the PostgreSQL cluster.

### Architecture

```
Client ──► Pooler Service ──► PgBouncer Pod(s) ──► cluster-rw / cluster-ro
```

- Each `Pooler` resource creates a separate **Deployment** of PgBouncer pods
- The pooler connects to the cluster's `-rw`, `-ro`, or `-r` service depending
  on `spec.type`
- Multiple poolers can exist per cluster (e.g., one `rw` and one `ro`)

### Configuration

| Setting | Options | Default |
|---|---|---|
| `spec.type` | `rw`, `ro`, `r` | — |
| `spec.instances` | Replica count | 1 |
| `spec.pgbouncer.poolMode` | `session`, `transaction` | `session` |
| `spec.pgbouncer.authQuery` | SQL for credential validation | Uses `user_search()` function |
| `spec.pgbouncer.parameters` | PgBouncer config overrides | — |
| `spec.pgbouncer.pg_hba` | Custom HBA rules | — |
| `spec.paused` | Pause all connections | `false` |

### TLS

PgBouncer supports full TLS:
- Server-side: PgBouncer → PostgreSQL (via `serverTLSSecret` / `serverCASecret`)
- Client-side: Client → PgBouncer (via `clientTLSSecret` / `clientCASecret`)

---

## Plugin System (CNPG-I)

CNPG-I (CloudNativePG Interface) is a **gRPC-based plugin architecture** that
allows extending the operator's functionality.

### Current State

- **Primary use case:** Backup and WAL archiving
- **Official plugin:** Barman Cloud Plugin (maintained by CloudNativePG community)
- **Transition:** Moving from monolithic backup code to plugin-first architecture
  (the native `spec.backup.barmanObjectStore` interface is deprecated as of v1.26)

### Plugin Discovery

Plugins communicate with the Instance Manager via:
- **Unix sockets** at `/var/run/cloudnative-pg/plugins/`
- **TCP endpoints** for remote plugins

The Instance Manager maintains persistent gRPC connections to discovered
plugins.

### Plugin Configuration

```yaml
spec:
  pluginConfiguration:
    - name: barman-cloud.cloudnative-pg.io
      isWALArchiver: true
      parameters:
        barmanObjectName: my-backup-config
```

---

## Monitoring & Observability

### Metrics

- **Port:** 9187 (HTTP or HTTPS)
- **Endpoint:** `/metrics` (Prometheus format)
- **Exporter:** Built into the Instance Manager (no separate exporter pod)
- **Execution context:** `pg_monitor` role, atomic transactions

### Default Metrics

Installed via the `cnpg-default-monitoring` ConfigMap:

| Category | Examples |
|---|---|
| PostgreSQL process | Uptime, connections, transactions |
| Replication | Lag (bytes), LSN positions, WAL receiver status |
| WAL archiving | Archive success/failure counts, ready files |
| Storage | PGDATA usage, WAL volume usage |
| Backup | Last backup time, backup duration |

### Caching

Query results are cached with a **30-second TTL** (configurable via
`spec.monitoring.metricsQueriesTTL`). Set to 0 to disable caching and run
queries on every scrape.

### Custom Metrics

Users can define custom metric queries via ConfigMaps or Secrets, referenced in
the `Cluster` spec. Custom queries can target specific databases and override
the default target database.

---

## Storage Architecture

### PVC Management

Since CNPG does **not** use StatefulSets, it manages PVCs directly:

- One PVC per pod for the main PGDATA directory
- Optional separate PVC for WAL storage (`spec.walStorage`)
- PVCs are created when pods are created and persist across pod restarts
- Dynamic volume expansion is supported (storage-class dependent)

### Data Layout

| Path | Contents | PVC |
|---|---|---|
| `/var/lib/postgresql/data` | PGDATA (tables, indexes, configs) | Main PVC |
| `/var/lib/postgresql/wal` | WAL segments (if separated) | WAL PVC (optional) |
| `/var/lib/postgresql/tablespaces/*` | Custom tablespace data | Additional PVCs |

### Volume Snapshots

For backup via volume snapshots:
- Uses the Kubernetes CSI VolumeSnapshot API
- Can be **online** (hot, no downtime) or **offline** (cold, pod stopped)
- Incremental snapshots depend on the underlying CSI driver
- Snapshot-based recovery is the fastest restore path

---

## Networking & Service Routing

### Label-Driven Service Selection

The operator manages a pod label that Services use as a selector:

```
Label key:   cnpg.io/instanceRole
Values:      primary | replica
```

**On failover/switchover:**
1. Operator updates the promoted pod's label to `primary`
2. Operator updates the demoted pod's label to `replica`
3. Kubernetes endpoint controller automatically re-routes the Services
4. Clients connected to the `-rw` service see transparent failover

### Services

| Service | Selector | Routes To |
|---|---|---|
| `{cluster}-rw` | `cnpg.io/instanceRole=primary` | Primary only (read-write) |
| `{cluster}-ro` | `cnpg.io/instanceRole=replica` | Replicas only (read-only) |
| `{cluster}-r` | All ready pods | Primary + Replicas (any read) |

Service templates can be customized via `spec.managed.services` in the Cluster
spec, allowing annotations, labels, and type overrides (e.g., `LoadBalancer`).

---

## Key Source Code Map

| Component | Path | Description |
|---|---|---|
| **Entry point** | `cmd/manager/main.go` | Multi-subcommand binary (74 lines) |
| **Cluster CRD** | `api/v1/cluster_types.go` | Cluster type definitions (~2700 lines) |
| **Backup CRD** | `api/v1/backup_types.go` | Backup/ScheduledBackup types |
| **Pooler CRD** | `api/v1/pooler_types.go` | Connection pooler types |
| **FailoverQuorum CRD** | `api/v1/failoverquorum_types.go` | Quorum tracking types |
| **Cluster controller** | `internal/controller/cluster_controller.go` | Main reconciliation loop (~1550 lines) |
| **Failover logic** | `internal/controller/replicas.go` | Primary/replica selection (~450 lines) |
| **Quorum logic** | `internal/controller/replicas_quorum.go` | R+W>N quorum evaluation |
| **Cluster status** | `internal/controller/cluster_status.go` | Status reconciliation |
| **Rolling updates** | `internal/controller/cluster_upgrade.go` | Switchover during upgrades |
| **Backup controller** | `internal/controller/backup_controller.go` | Backup reconciliation |
| **Pooler controller** | `internal/controller/pooler_controller.go` | PgBouncer management |
| **Instance Manager** | `internal/cmd/manager/instance/run/cmd.go` | In-pod runtime setup (436 lines) |
| **Instance controller** | `internal/management/controller/instance_controller.go` | PG lifecycle (~1430 lines) |
| **pg_rewind** | `pkg/management/postgres/instance.go` | Rewind/rejoin logic |
| **Status types** | `pkg/postgres/status.go` | PostgresqlStatus, LSN sorting |
| **Fencing** | `pkg/utils/fencing.go` | Fence/unfence annotation management |
| **PDB specs** | `pkg/specs/poddisruptionbudget.go` | PDB generation |
| **Role labels** | `pkg/reconciler/instance/metadata.go` | Pod label management |
| **Status patching** | `pkg/resources/status/patch.go` | Optimistic-lock status updates |
| **Replication config** | `pkg/postgres/replication/replication.go` | Sync standby name generation |
| **Plugin repository** | `internal/cnpi/plugin/repository/setup.go` | CNPG-I plugin discovery |
