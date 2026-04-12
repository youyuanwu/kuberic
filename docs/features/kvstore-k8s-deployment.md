# KVStore Kubernetes Deployment

Deploy the kvstore example as a KubelicateSet in KinD, matching the
existing xdata-app deployment pattern.

## Current State

The xdata-app stack deploys to KinD via:

```
CMake targets → cargo build → docker build → kind load → kubectl apply
```

Components:
- **xdata-operator**: Deployment (1 replica) watching `XdataApp` CRD
- **xdata-app**: StatefulSet (3 replicas) managed by the operator
- **kind-config.yaml**: NodePort mappings 30081–30083

The kubelicate-operator has its own CRD (`KubelicateSet`) and reconciler
that creates bare Pods (not StatefulSets). It manages three ports per pod:
app (8080), control (9090), data (9091).

**kvstore** currently runs only in-process tests — no container or K8s
manifests exist.

## Goal

Run kvstore as a `KubelicateSet` in KinD so the kubelicate-operator
manages its lifecycle (role assignment, failover, replication) on real
Kubernetes pods.

## Architecture

```
┌──────────────────────────────────────────────────────┐
│  KinD Cluster                                        │
│                                                      │
│  ┌──────────────────────┐                            │
│  │ kubelicate-operator   │ watches KubelicateSet      │
│  │ (Deployment, 1 pod)  │ creates pods + services    │
│  └──────┬───────────────┘                            │
│         │ gRPC control (9090)                        │
│         ▼                                            │
│  ┌─────────────┐ ┌─────────────┐ ┌────────────┐     │
│  │ kvstore-0   │ │ kvstore-1   │ │ kvstore-2  │     │
│  │ Primary     │ │ Secondary   │ │ Secondary  │     │
│  │ :8080 app   │ │ :8080 app   │ │ :8080 app  │     │
│  │ :9090 ctrl  │ │ :9090 ctrl  │ │ :9090 ctrl │     │
│  │ :9091 data  │ │ :9091 data  │ │ :9091 data │     │
│  └─────────────┘ └─────────────┘ └────────────┘     │
│         ▲                 ▲              ▲           │
│  ┌──────┴─────────────────┴──────────────┴──────┐    │
│  │  Services (ClusterIP, label-driven)          │    │
│  │  kvstore-rw  → role=primary                  │    │
│  │  kvstore-ro  → role=secondary                │    │
│  │  kvstore-r   → all pods                      │    │
│  └──────────────────────────────────────────────┘    │
└──────────────────────────────────────────────────────┘
         │
    kubectl port-forward (test access)
```

## Deployment Artifacts

### 1. Dockerfile — `examples/kvstore/deploy/Dockerfile`

Follow xdata-app pattern: ubuntu:24.04 base, copy pre-built debug
binary, non-root user (UID 10000), expose ports.

```dockerfile
FROM ubuntu:24.04
RUN apt-get update && \
    apt-get install -y --no-install-recommends ca-certificates && \
    rm -rf /var/lib/apt/lists/*
RUN useradd -m -u 10000 appuser
WORKDIR /app
COPY target/debug/kvstore /app/kvstore
RUN chown -R appuser:appuser /app
USER appuser
EXPOSE 8080 9090 9091
CMD ["/app/kvstore"]
```

### 2. KubelicateSet CRD manifests

The kubelicate-operator already has a `KubelicateSet` CRD and reconciler
that creates pods with the right ports. We only need an example CR.

**`examples/kvstore/deploy/kubelicateset.yaml`**:

```yaml
apiVersion: kubelicate.io/v1
kind: KubelicateSet
metadata:
  name: kvstore
  namespace: xedio
spec:
  replicas: 3
  image: localhost/kvstore:latest
  port: 8080
  controlPort: 9090
  dataPort: 9091
  failoverDelay: 5
```

### 3. KinD config — `deploy/kind-config.yaml` (repo root)

Single canonical config for the whole repo. Minimal — no NodePort
mappings. All test access uses `kubectl port-forward`.

```yaml
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
```

### 4. CMake targets — `examples/kvstore/CMakeLists.txt`

```cmake
add_custom_target(kvstore-image ALL
    COMMAND docker build -t localhost/kvstore
        -f examples/kvstore/deploy/Dockerfile .
    COMMAND ${KIND_EXE} load docker-image localhost/kvstore:latest
        --name kind
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
    DEPENDS build_rust_bins
)

add_custom_target(kvstore-deploy
    COMMAND kubectl apply
        -f examples/kvstore/deploy/kubelicateset.yaml
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)

add_custom_target(kvstore-delete
    COMMAND kubectl delete
        -f examples/kvstore/deploy/kubelicateset.yaml
    WORKING_DIRECTORY ${CMAKE_SOURCE_DIR}
)
```

Root `CMakeLists.txt` gets: `add_subdirectory(examples/kvstore)`.

### 5. CI changes — `.github/workflows/CI.yml`

Update the kind config path (or keep shared). Add kvstore image build
alongside xdata images. The kubelicate-tests crate can use the same
`ensure_*_deployed()` pattern for kvstore.

## kvstore Binary Changes

The binary needs fixed port binding for container use. Current defaults
are `127.0.0.1:0` (random port, localhost-only) — unsuitable for K8s.

Required changes to `main.rs` CLI args:

| Arg | Current Default | K8s Default | Reason |
|-----|----------------|-------------|--------|
| `--control-bind` | `127.0.0.1:0` | `0.0.0.0:9090` | Operator reaches pod via pod IP |
| `--data-bind` | `127.0.0.1:0` | `0.0.0.0:9091` | Peer pods reach each other |
| `--client-bind` | `127.0.0.1:0` | `0.0.0.0:8080` | Clients reach via Service |
| `--replica-id` | `1` | From env | Operator sets per-pod env var |

The pod spec in the kubelicate-operator reconciler sets container args.
Currently `build_pod()` doesn't set args — the kvstore binary needs
sensible defaults or the reconciler needs to pass `--control-bind`,
`--data-bind`, `--client-bind`, and `--replica-id` as container args.

**Approach**: Keep `127.0.0.1:0` defaults for backward compatibility
with existing tests. Override via env vars in the container spec:

```json
"env": [
  { "name": "KUBELICATE_CONTROL_BIND", "value": "0.0.0.0:9090" },
  { "name": "KUBELICATE_DATA_BIND",    "value": "0.0.0.0:9091" },
  { "name": "KUBELICATE_CLIENT_BIND",  "value": "0.0.0.0:8080" }
]
```

The kvstore binary reads these env vars with CLI args as fallback.
Existing tests continue to use `127.0.0.1:0` defaults unchanged.

**Replica ID convention**: The `KUBELICATE_REPLICA_ID` env var uses
**1-based** IDs (matching the driver's `ReplicaId = pod_index + 1`).
The pod label `kubelicate.io/replica-id` stores 0-based pod index.
These are distinct concepts: pod index (0-based, for PVC/pod naming)
vs replica ID (1-based, for quorum/fencing).

```json
{ "name": "KUBELICATE_REPLICA_ID", "value": "<pod_index + 1>" }
```

## Services (CNPG Pattern)

Follow CNPG's label-driven service routing instead of per-pod NodePort
services. The operator manages pod labels; Kubernetes endpoint controller
automatically re-routes traffic on failover.

### Pod Labels (set by operator)

```
kubelicate.io/set: kvstore
kubelicate.io/role: primary | secondary
```

On failover/switchover, the operator updates labels in a specific
order to minimize zero-endpoint windows:

1. **Promote first**: Set new primary's label to `primary`
2. **Demote second**: Set old primary's label to `secondary` (or remove)

This ensures the `-rw` service always has at least one endpoint
during the transition. Crash failover (old primary unreachable) has
an unavoidable zero-endpoint window until the new primary is labeled.

Label patch errors must be propagated (not silently swallowed) since
a failed label update leaves the service pointing at the wrong pod.

### Services (created by operator)

The operator creates 3 ClusterIP services per KubelicateSet during
the `Creating` phase. This requires extending the `ClusterApi` trait
with service CRUD methods (`create_service`, `get_service`,
`delete_service`, `list_services`) and corresponding test mocks.

| Service | Selector | Routes To |
|---------|----------|-----------|
| `kvstore-rw` | `kubelicate.io/set=kvstore, kubelicate.io/role=primary` | Primary only |
| `kvstore-ro` | `kubelicate.io/set=kvstore, kubelicate.io/role=secondary` | Secondaries only |
| `kvstore-r` | `kubelicate.io/set=kvstore` | All pods |

All are ClusterIP services (no NodePort). Port mappings:
- Port 8080 → client gRPC
- Port 9090 → control gRPC (operator use)
- Port 9091 → data gRPC (inter-pod replication)

### Test Access via `kubectl port-forward`

Tests on the host use service-name-based `kubectl port-forward` with
local port 0 (OS picks a free port). No NodePort, no kind
extraPortMappings, no port conflicts — even with parallel CI jobs.

```rust
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use std::process::Stdio;

/// Start a port-forward to a Kubernetes service.
/// Uses local port 0 — OS assigns a free port.
/// Returns the child process and the assigned local port.
/// Times out after 10s if kubectl fails to bind.
async fn port_forward_svc(ns: &str, svc: &str, remote: u16) -> (Child, u16) {
    let mut child = Command::new("kubectl")
        .args([
            "port-forward", "-n", ns,
            &format!("svc/{svc}"),
            &format!(":{remote}"),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("kubectl port-forward");

    // First line: "Forwarding from 127.0.0.1:<port> -> <remote>"
    // or "Forwarding from [::1]:<port> -> <remote>" (IPv6)
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();

    let read_result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reader.read_line(&mut line),
    ).await.expect("port-forward timed out — kubectl may have failed to bind");
    read_result.unwrap();

    // Parse assigned port — handles both IPv4 and IPv6 output
    let port: u16 = line
        .rsplit("]:").next()                          // try [::1]:<port>
        .or_else(|| line.split("127.0.0.1:").nth(1))  // try 127.0.0.1:<port>
        .and_then(|s| s.split_whitespace().next())
        .expect("failed to parse port from kubectl output")
        .parse()
        .expect("port is not a number");

    (child, port)
}
```

**Test fixture** (once per suite, not per test):

```rust
// Suite setup — OS picks free local ports
let (rw, rw_port) = port_forward_svc("xedio", "kvstore-rw", 8080).await;
let (ro, ro_port) = port_forward_svc("xedio", "kvstore-ro", 8080).await;

// Tests connect to dynamically assigned ports
let client = connect_kv_client(&format!("http://localhost:{rw_port}")).await;
client.put("key", "value").await;

// Teardown
rw.kill().await;
ro.kill().await;
```

**Failover tests**: After triggering failover, the `-rw` forward's
existing TCP connection breaks (old pod died). Re-establish the
forward to pick up the new primary:
```rust
rw.kill().await;
let (rw, rw_port) = port_forward_svc("xedio", "kvstore-rw", 8080).await;
// reconnect client at new port — now talks to new primary
```

## Data Address Registration (already implemented)

The operator already reads `pod.Status.PodIP` from the Kubernetes API
and constructs `GrpcReplicaHandle` addresses in
`create_replica_handle()` and `build_member_status()`. No additional
work needed here.

**Invariant**: `pod.Status.PodIP` is empty until the pod is scheduled
and the network is configured. Only read PodIP from pods that pass
`is_pod_ready()` — the existing reconciler already enforces this via
the `ready_pods` filter. The `build_replica_handle` implementation
must return `None` (not panic) if PodIP is absent:

```rust
fn build_replica_handle(pod: &Pod, spec: &KubelicateSetSpec) -> Option<GrpcReplicaHandle> {
    let ip = pod.status.as_ref()?.pod_ip.as_deref().filter(|s| !s.is_empty())?;
    let control = format!("http://{}:{}", ip, spec.control_port);
    let data    = format!("http://{}:{}", ip, spec.data_port);
    Some(GrpcReplicaHandle::new(control, data))
}
```

## Persistent Storage (PVC)

Following CNPG's pattern, the operator manages PVCs directly (one per
pod). PVCs persist across pod restarts so kvstore can recover state
from disk.

### PVC Lifecycle

```
Pod created → PVC created alongside (same name, owner reference)
Pod dies    → PVC survives (data preserved)
Pod recreated → PVC re-attached → kvstore recovers from --data-dir
PVC also gone → fresh PVC → rebuilt via copy from primary
```

### Operator `build_pod()` Changes

The operator creates a PVC per pod and mounts it into the container:

```json
{
  "volumes": [{
    "name": "data",
    "persistentVolumeClaim": { "claimName": "kvstore-0-data" }
  }],
  "containers": [{
    "name": "app",
    "volumeMounts": [{
      "name": "data",
      "mountPath": "/var/lib/kvstore/data"
    }]
  }]
}
```

PVC spec (created separately, before the pod). Note: PVCs use
**labels** for association but no `ownerReferences` — this prevents
cascade deletion when the KubelicateSet CR is deleted (matching CNPG
pattern where PVCs outlive the cluster CR for data safety):

```yaml
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: kvstore-0-data
  namespace: xedio
  labels:
    kubelicate.io/set: kvstore
    kubelicate.io/pod-index: "0"
spec:
  accessModes: [ReadWriteOnce]
  resources:
    requests:
      storage: 256Mi
```

### CRD Extension

Add `storage` field to `KubelicateSetSpec`:

```rust
/// Storage size per pod PVC.
#[serde(default = "default_storage")]
pub storage: String,  // e.g. "256Mi"

/// PVC retention policy on CR deletion: Delete (default) or Retain.
#[serde(default = "default_pvc_retention")]
pub pvc_retention_policy: PvcRetentionPolicy,
```

### Recovery Scenarios

| Scenario | PVC State | Action |
|----------|-----------|--------|
| Pod crash/restart | PVC exists | Re-attach PVC, kvstore reads existing data |
| Pod deleted | PVC exists | Operator creates new pod, attaches PVC |
| PVC deleted | Gone | Operator creates fresh PVC + pod, copy from primary rebuilds state |
| Node failure | PVC stuck (RWO) | Fresh PVC on new node, copy from primary (see Open Questions) |
| CR deleted | PVCs remain | PVCs have no ownerRef — manual cleanup or retention policy |

### Reconcile Loop Integration

The reconciler creates PVCs alongside pods and handles dangling PVCs
(PVCs without pods) on each reconcile cycle.

**`Phase::Pending` → `Phase::Creating`** (initial deployment):

```rust
async fn create_pods(api: &dyn ClusterApi, set: &KubelicateSet, ns: &str)
    -> Result<(), String>
{
    for index in 0..set.spec.replicas {
        ensure_pvc(api, set, ns, index).await?;
        ensure_pod(api, set, ns, index).await?;
    }
    Ok(())
}

async fn ensure_pod(api: &dyn ClusterApi, set: &KubelicateSet, ns: &str, index: i32)
    -> Result<(), String>
{
    let name = format!("{}-{}", set.name_any(), index);
    if api.get_pod(ns, &name).await.is_ok() {
        return Ok(());  // pod already exists (crash recovery)
    }
    let pod = build_pod(set, ns, index);
    api.create_pod(ns, &pod).await
}

async fn ensure_pvc(api: &dyn ClusterApi, set: &KubelicateSet, ns: &str, index: i32)
    -> Result<(), String>
{
    let name = format!("{}-{}-data", set.name_any(), index);
    if api.get_pvc(ns, &name).await.is_ok() {
        return Ok(());  // PVC already exists (pod restart case)
    }
    let pvc = build_pvc(set, ns, index);
    api.create_pvc(ns, &pvc).await
}
```

Both `ensure_pvc` and `ensure_pod` are idempotent — safe to re-run
after operator crash mid-creation.

**`Phase::Healthy`** (steady-state reconcile):

```rust
// Check for dangling PVCs (PVC exists, pod missing)
let pvc_label = format!("kubelicate.io/set={}", name);
let pvcs = api.list_pvcs(&namespace, &pvc_label).await?;
for pvc in &pvcs {
    let index = pvc.labels()
        .get("kubelicate.io/pod-index")
        .and_then(|s| s.parse::<i32>().ok())
        .expect("PVC missing pod-index label");
    if !pod_exists_for_index(&pods, index) {
        info!(pvc = %pvc.name_any(), "dangling PVC, recreating pod");
        ensure_pod(api, set, &namespace, index).await?;
    }
}

// Scale-up: create PVCs + pods for new indices
for index in pods.len() as i32..set.spec.replicas {
    ensure_pvc(api, set, &namespace, index).await?;
    ensure_pod(api, set, &namespace, index).await?;
}
```

**RBAC update** — add `create` and `delete` to PVC permissions, add
Service management:

```yaml
- apiGroups: [""]
  resources: ["persistentvolumeclaims"]
  verbs: ["get", "list", "watch", "create", "delete"]
- apiGroups: [""]
  resources: ["services"]
  verbs: ["get", "list", "watch", "create", "update", "patch", "delete"]
```

**ClusterApi trait expansion** — add methods for PVC and Service
management to maintain the testability abstraction:

```rust
// New methods needed on ClusterApi trait
async fn get_pvc(&self, ns: &str, name: &str) -> Result<PVC, String>;
async fn create_pvc(&self, ns: &str, pvc: &PVC) -> Result<(), String>;
async fn list_pvcs(&self, ns: &str, label: &str) -> Result<Vec<PVC>, String>;
async fn delete_pvc(&self, ns: &str, name: &str) -> Result<(), String>;
async fn create_service(&self, ns: &str, svc: &Service) -> Result<(), String>;
async fn get_service(&self, ns: &str, name: &str) -> Result<Service, String>;
async fn delete_service(&self, ns: &str, name: &str) -> Result<(), String>;
```

**Scale down**: Delete pod first, then optionally delete PVC. Keep PVC
by default for quick scale-up recovery (data still on disk).

### Security Context

Since the container runs as non-root (UID 10000), the PVC mount needs
proper permissions. Use `fsGroup` in the pod security context:

```json
"securityContext": {
  "runAsNonRoot": true,
  "runAsUser": 10000,
  "fsGroup": 10000
}
```

### Readiness and Liveness Probes

Following xdata-app's pattern, add TCP probes to `build_pod()`. The
operator's `is_pod_ready()` check depends on readiness probes —
without them, pods are marked Ready before gRPC servers are listening.

```json
"readinessProbe": {
  "tcpSocket": { "port": 9090 },
  "initialDelaySeconds": 5,
  "periodSeconds": 5,
  "timeoutSeconds": 3,
  "failureThreshold": 2
},
"livenessProbe": {
  "tcpSocket": { "port": 9090 },
  "initialDelaySeconds": 10,
  "periodSeconds": 10,
  "timeoutSeconds": 5,
  "failureThreshold": 3
}
```

## Deployment Workflow

```
# One-time setup
cmake --build build --target create-kind-cluster

# Build and load images
cmake --build build --target all
  → builds rust binaries
  → builds kvstore docker image
  → builds kubelicate-operator docker image
  → loads both into kind

# Deploy
cmake --build build --target kubelicate-operator-deploy
cmake --build build --target kvstore-deploy

# Verify
kubectl get kubelicatesets -n xedio
kubectl get pods -n xedio
```

## Implementation Order

Split into two PRs to separate framework changes from kvstore-specific
deployment.

### PR 1: Operator Infrastructure

Framework-level changes testable with existing mock `KvClusterApi`
(add in-memory `HashMap` for PVCs/Services — no K8s cluster needed).

1. **ClusterApi trait expansion** — Add PVC + Service methods to trait.
   Mock impls store in `HashMap<String, PVC/Service>`.
2. **`build_pod()` typed builders** — Refactor from `serde_json::json!`
   to `k8s_openapi` typed Pod construction.
3. **Readiness/liveness probes** — TCP probes in `build_pod()`.
4. **PVC management** — `ensure_pvc()`, `ensure_pod()` idempotent
   creation, dangling PVC detection, `storage` + `pvc_retention_policy`
   CRD fields.
5. **Service management** — Create rw/ro/r ClusterIP services in
   `Creating` phase, label update ordering on failover
   (promote-first, demote-second).
6. **Env vars in pod spec** — Replica ID (1-based), bind address
   overrides, RUST_LOG.

Mock tests (no K8s cluster):

**Extend existing tests** (add assertions to current lifecycle tests):

- `test_reconciler_creates_partition_and_serves_kv`: Assert 3 PVCs
  created alongside 3 pods, 3 services (rw/ro/r) created in Creating
  phase. Verify PVC names match `{set}-{index}-data` pattern.
- `test_reconciler_detects_primary_failure_and_fails_over`: Assert
  role labels updated in correct order (new primary labeled `primary`
  before old primary labeled `secondary`).
- `test_reconciler_scale_up`: Assert `ensure_pvc` called for new
  replica before pod creation.
- `test_reconciler_scale_down`: Assert pod deleted, PVC retained.

**New tests** (behaviors not covered by existing tests):

- `test_reconciler_dangling_pvc_recreates_pod`: Remove a pod from
  mock without removing its PVC. Reconcile in Healthy phase. Assert
  new pod created referencing existing PVC.
- `test_reconciler_idempotent_creation`: Reconcile Creating phase
  twice. Assert no duplicate PVCs or pods (ensure_pvc/ensure_pod
  skip existing).
- `test_reconciler_pvc_retention_delete`: Delete CR with
  `pvcRetentionPolicy: Delete`. Assert PVCs cleaned up in Deleting
  phase finalizer.
- `test_reconciler_pvc_retention_retain`: Delete CR with
  `pvcRetentionPolicy: Retain`. Assert PVCs survive CR deletion.

### PR 2: KVStore K8s Deployment

Kvstore-specific deployment artifacts + real K8s integration tests.

1. **Dockerfile + CMake** — kvstore image builds and loads into kind.
2. **Binary env var support** — Read `KUBELICATE_*_BIND` and
   `KUBELICATE_REPLICA_ID` env vars (keep current defaults).
3. **KinD config** — Single canonical config at `deploy/kind-config.yaml`.
4. **KubelicateSet CR** — Example manifest for kvstore.
5. **kubelicate-tests** — K8s integration tests using service-name
   port-forward with timeout + IPv6 support.

## Open Questions

1. **Operator CRD extensions**: Should `KubelicateSetSpec` grow fields
   for `env`/custom volume mounts like the `XdataApp` CRD, or keep
   it minimal?

2. ~~**RWO volume fencing on node failure**~~: Deferred — scope PVC
   support to single-node KinD for v1. Multi-node RWO fencing is a
   future concern.

3. ~~**PVC retention policy on CR deletion**~~: Resolved — add
   `pvcRetentionPolicy` field to CRD, default `Delete`. Values:
   `Delete` (operator cleans up PVCs on CR deletion) or `Retain`
   (PVCs survive for manual recovery). Operator checks policy in
   the `Deleting` phase finalizer.

4. ~~**Stale-epoch PVC reattach**~~: Resolved — not an issue. When a
   pod restarts (with stale PVC), the driver calls `restart_secondary`
   → `add_replica` → `build_replica`, which runs the full copy
   protocol from the primary. The copy stream overwrites whatever is
   on disk. Stale PVC data is harmless — it's replaced entirely.

5. ~~**`build_pod()` typed builders**~~: Resolved — refactor to
   `k8s_openapi` typed builders when adding volumes, probes, env,
   and securityContext. The current `serde_json::json!` blob is
   already ~30 lines and will double. Typed builders catch field
   name typos at compile time. Do this as part of step 4/5 in the
   implementation order (not a separate phase).

6. ~~**KinD config consolidation**~~: Resolved — move to a single
   canonical config at repo root (`deploy/kind-config.yaml`). Use
   minimal config (no NodePort mappings). Migrate xdata-app tests to
   port-forward. Update `CMakeLists.txt` `create-kind-cluster` target.
