# Kuberic Test Strategy

Kuberic validation is split into exact nextest tiers, workspace checks,
provider-backed smoke tests, and explicitly owned Kubernetes scenarios.

## Workspace Checks

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --doc --workspace --all-features
scripts/check_runtime_public_api.sh
```

The API guard compares documented and source-public inventories and runs an
external privacy fixture. It protects the public interface boundary without
exporting host capabilities.

## Exact Nextest Inventory

```bash
just nextest-validate
```

The current test inventory is:

| Tier | Tests |
|---|---:|
| ordinary | 291 |
| PostgreSQL | 41 |
| KinD live | 3 |
| build-only | 2 |
| **total** | **337** |

Ordinary partition counts are `73, 73, 73, 72`. PostgreSQL partition counts
are `11, 13, 8, 9`. The validator proves every test has exactly one tier and
every configured partition union is complete.

## Ordinary Tier

```bash
just nextest-test
```

The ordinary tier is cluster-free. It includes runtime/controller behavior,
real SQLite and KVStore2 providers, immutable old-state rejection, Public V1
wire/history/session/evidence coverage, task reclamation, writer fencing, and
the structural KSF catalog.

Timeout-sensitive hosted recovery tests should use the repository nextest
profile rather than an ad-hoc highly parallel `cargo test` invocation.

### In-Process Controller/Application Journeys

```bash
cargo nextest run -p kuberic-level-tests --all-features --profile ordinary \
  -E 'test(in_process::journeys)'
```

The [in-process harness](../../proposal/in-process-system-test-harness.md)
connects the production reconciler and Kubernetes adapter to real agent RPCs,
hosted KVStore2 replicas, TCP replication, and durable files. Only Kubernetes
resource storage and Pod/PVC realization are simulated. It requires no cluster,
containers, kubeconfig, or application binaries.

Five ordinary-tier journeys cover bootstrap with a status conflict, contained
restart/session renewal, secondary replacement with delayed deletion,
stopped-primary failover, and controller recreation after an operation is
persisted. Scenarios call `KvService` directly rather than its HTTP router.
Failover admits the surviving two-member quorum; it does not prove fencing of
a frozen or partitioned process.

Each scenario owns dynamically allocated loopback listeners, resources, and
storage. Callers pass a bound control socket at startup, not host construction.
One server task owns it across initialization and established agent serving,
with no close/rebind or shared listener ownership. The replicator binds during
Open and the controller discovers its returned endpoint through the real agent
report. Teardown drains hosts and application consumers before releasing storage;
failures retain the data directory and report its path. Deployment, Service
networking, watch delivery, and process pause/resume remain KinD concerns.

## Build-Only Tier

```bash
just nextest-build-only
```

This runs:

- the runtime API/privacy guard;
- the ignored executable Public V1 conformance gate.

The conformance gate executes the registered replacement scenarios under one
bounded parent deadline and checks that every KSF mapping selects an existing
test.

## PostgreSQL

Routine validation:

```bash
just nextest-postgres-smoke
```

The smoke target builds the owned command binary and runs eight representative
Public V1 custom-Replicator scenarios. Run `just nextest-postgres` only when
the full PostgreSQL suite is explicitly required.

## Archive Reproducibility

```bash
just nextest-archive
just nextest-validate-archive
```

The archive is built from the current all-feature workspace after the
build-only gate. Archive validation repeats exact tier and partition
accounting against the packaged binaries.

## Focused Public V1 Coverage

Important focused targets include:

- `kuberic-controller::public_v1_production`: fresh resource bootstrap and
  preseeded-report ordering, exact replacement-incarnation selection,
  contained restart/session renewal, and failover-delay boundaries;
- `kuberic-runtime::public_v1_wire`: protocol-10 rejection and serialized
  `identity` fields;
- `kuberic-runtime::public_v1_conformance`: KSF catalog and ownership
  mutations;
- `kuberic-runtime::public_v1_recovery`: lifecycle, build, failover,
  switchover, containment, task, and writer behavior;
- `kuberic-runtime::public_v1_session_renewal`: durable stage replay,
  participant alignment, monotonic journal supersession, and exact fencing;
- SQLite/KVStore2 `provider_state_rejection`: complete tree/byte immutability
  for old provider state.

## Owned KinD Scenarios

```bash
export KIND_CLUSTER_NAME=kuberic-level-dev
mkdir -p target/kuberic-level-dev
export KUBECONFIG="$PWD/target/kuberic-level-dev/kubeconfig"
export KUBE_CONTEXT="kind-${KIND_CLUSTER_NAME}"
export KUBERIC_AGENT_BEARER_TOKEN=local-level-triggered-token

just create-kind-cluster
just level-triggered-images
just level-triggered-install
just level-triggered-kind-test bootstrap replacement failover
just delete-kind-cluster
```

The harness requires an explicit ownership receipt and refuses default/shared
cluster coordinates. The three scenarios inspect current Public V1 authority,
resources, agent diagnostics, and exactly one granted writer. Bootstrap proves
quorum writes after a contained process restart; replacement proves preserved
data and exact new-incarnation admission; failover uses a sustained containerd
task pause and proves preserved data, post-failover quorum writes, resumed
old-primary fencing, and stale-write rejection. The scenarios do not read
removed legacy status fields.
