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
| ordinary | 350 |
| PostgreSQL | 41 |
| KinD live | 3 |
| build-only | 2 |
| **total** | **396** |

Ordinary partition counts are `88, 88, 87, 87`. PostgreSQL partition counts
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

### Built-In Service Operation Delivery

```bash
cargo nextest run -p kuberic-runtime -p kvstore2 -p sqlite-replicated \
  --all-features --profile ordinary --test-threads 1 \
  -E 'test(hosted_operation_stream_) or test(stream_) or test(sqlite_operation_stream_copy_preserves_verified_uncommitted_suffix)'
```

Thirteen hosted factory/service tests hold real copy, copy-complete, active and
retained replication operations. They cover delayed durable ACK, duplicate
commitment, rejection/drop/invalid history, exact retries, cancellation,
authority/role/epoch/configuration fencing, synchronous-effect drainage,
shutdown without consumers, and primary-local no-redelivery. Memory-backed
hosted tests prove sequencing, not fsync; real KV/SQLite consumer tests verify
durable commitment without duplicate application and SQLite cancellation.
Deterministic admission-race tests complement hosted active-effect drainage.
A real SQLite factory/service build preserves an intact uncommitted suffix
over copy, verifies per-frame ACK/retry without reapplication, and prevents
post-commit rejection from supplying successful replay or terminal proof.

### In-Process Controller/Application Journeys

```bash
cargo nextest run -p kuberic-level-tests --all-features --profile ordinary \
  -E 'test(in_process::journeys)'
```

The [in-process harness](../../proposal/in-process-system-test-harness.md)
connects the production reconciler and Kubernetes adapter to real agent RPCs,
hosted KVStore2 replicas, TCP replication, and durable files. Only Kubernetes
resource storage, Pod/PVC realization, and explicit socket faults are simulated. It requires no cluster,
containers, kubeconfig, or application binaries.

Twenty-four ordinary-tier journey/assertion tests cover bootstrap/status conflict,
support Service/Secret repair, contained restart, replacement with delayed exact
cleanup, replacement followed by same-storage restart, stopped-primary failover,
live-primary isolation/reconnection, quorum loss/restoration, and spec-driven
switchover. Controller recreation cuts cover persisted intent and durable
completion before status advancement for replacement and failover, plus prepared
and completed switchover handoff. Provider LSNs/snapshots detect duplicate logical
effects; exact accepted completions must be consumed once. Scenarios call
`KvService` directly rather than its HTTP router.

Readiness requires no pending accepted operation, transition, replacement,
session renewal, or planned transfer; exact admitted/live session, creation and storage agreement;
canonical configuration evidence; and settled access for all live hosts,
including excluded predecessors. Admitted committed and applied progress must
agree. Survivor observations explicitly exclude only
acknowledged isolated hosts. Rejected real application writes, not stale
inaccessible reports, establish stale-writer safety. Timed-out writes remain
ambiguous; restored members must agree on an absent or attempted value.

Owned TCP proxies cut existing duplex streams and new source/destination
connections at both replication and Build dial paths, including controller
control traffic, without replacing the isolated host/service/session. Unrelated
connections survive cuts and restoration. Fresh-Build fault probes verify zero
provider effects while blocked and transport reconnection, not automatic retry
of a failed Build. Negative-path tests drain active cuts and entered barriers.

Switchover cancellation/retargeting after durable peer preparation and controller
recreation must complete the frozen transfer before considering the edited spec.
Completion identifies that original request; both transfers preserve receipts.
An unaccepted request must not block failover when its source is already absent.

Live-primary return is exercised after genuine session renewal both with and
without rejected writes on the isolated host. The clean-tail case must fence and
recover the same host/session through retained-log catch-up, without snapshot
Build or a manufactured ambiguous suffix. Renewal projection cannot override
newer explicit Epoch/Role work or a conflicting same-epoch role.

Each scenario owns dynamically allocated loopback listeners, resources, and
storage. Callers pass a bound control socket at startup, not host construction.
One server task owns it across initialization and established agent serving,
with no close/rebind or shared listener ownership. The replicator binds during
Open and the controller discovers its returned endpoint through the real agent
report. A testing-feature hook routes physical sockets using the real canonical
source/destination identities, without changing commands or protocol payloads.
Teardown joins forwarding tasks, hosts and application consumers before releasing storage;
failures retain the data directory and report its path. Deployment, Service
networking, watch delivery, and process pause/resume remain KinD concerns.
Live network isolation is not scheduler suspension: the retained KinD failover
journey still pauses and resumes the original containerd task.

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
