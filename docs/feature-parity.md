# Archived Kuberic feature-parity tracker

This tracks capabilities delivered by the archived experimental stack that are
not yet integrated into the active fube-based Kuberic stack. It is not a claim
that archived Kuberic was production-ready, or that fube lacks every required
primitive. Archive links are evidence, not instructions to restore its runtime
or control protocol.

All unchecked items are open. Check an item only when its completion boundary
is supported end to end by active Kuberic, and update its current-scope
documentation at the same time.

## Recovery and lifecycle

- [ ] **GAP-01: FM/operator restart recovery.** Archived Kuberic persisted
  operation and topology intent and supported bounded controller recreation,
  including accepted handoffs. Current FM authority, native sessions and
  cleanup receipts are in memory; a new operator blocks existing allocations.
  Completion means safely reconstructing ownership and resuming supported
  operations after process replacement without duplicate authority or invented
  completion/cleanup evidence. Saving FM structs or trusting CRD status alone
  is insufficient.
  Evidence: [archived operation-resume test](../archive/kuberic-level-tests/src/in_process/journeys.rs#L1211-L1249),
  [archived handoff contract](../archive/docs/features/kuberic/level-triggered-operator.md#planned-switchover),
  [current restart contract](../kuberic-operator/README.md#restart-and-shutdown-contract).
  Proposed design: [restart-safe operator and FM ownership](operator-restart-design.md),
  covering minimal KubericSet authority, watch-driven reconciliation and
  required upstream fube work. The [state contract](operator-restart-state.md)
  specifies fields, commit/reconstruction rules and current FM field mapping.
  Local SF/CNPG evidence and alternatives are in the
  [background research](background/operator-restart-research.md).

- [ ] **GAP-02: Persistent replicas and same-storage restart.** Archived
  Kuberic used PVC/provider state, durable lifecycle authority and fresh
  process-session renewal. Current Kuberic supports only volatile replicas;
  application-crash repair creates and copies a fresh replica on the original
  Pod. Completion means a supported persisted service profile, storage
  lifecycle and safe existing-state restart with exact incarnation/session
  fencing. A copied volatile replacement is not persisted Reopen.
  Evidence: [archived durable ownership](../archive/kuberic-runtime/README.md#durable-ownership),
  [archived retained-storage restart test](../archive/kuberic-level-tests/src/in_process/journeys.rs#L802-L851),
  [current restoration](../README.md#replica-restoration).

- [ ] **GAP-03: Pod replacement and redundancy restoration.** Archived Kuberic
  supported same-cardinality replacement through new Pod/storage identities,
  build, admission and exact predecessor retirement. Current repair requires
  the original Pod, Node and control owners; Pod/Node loss or changed bindings
  block the deployment. Completion means safely replacing a lost Pod/native
  Node incarnation, copying and admitting its successor, and retiring the
  predecessor without accepting stale ownership. Physical Kubernetes Node
  rescheduling and placement policy need separately stated bounds.
  Evidence: [archived replacement contract](../archive/docs/features/kuberic/level-triggered-operator.md#topology-and-session-convergence),
  [archived replacement test](../archive/kuberic-level-tests/src/in_process/journeys.rs#L854-L920),
  [current bootstrap authority](../README.md#bootstrap-authority).

- [ ] **GAP-04: Observation-based outage failover and safe rejoin.** Archived
  Kuberic had delayed observation-based failover and bounded real-transport
  isolation/healing scenarios with an old Primary still alive. Current
  failover requires an exact native application-loss report; lost control
  channels and network partitions block rather than authorize promotion.
  Completion means native-safe failure detection, quorum/history-based
  reconfiguration and reconnect/rejoin with stale-writer protection. Service
  routing withdrawal, Pod readiness and TCP failure are not writer fencing.
  Evidence: [archived failover contract](../archive/docs/features/kuberic/level-triggered-operator.md#failover),
  [archived live-primary isolation test](../archive/kuberic-level-tests/src/in_process/journeys.rs#L1278-L1388),
  [current failover scope](../README.md#primary-application-crash-failover).

## Service and application capabilities

- [ ] **GAP-05: Configurable replica sizing and resize.** Archived Kuberic had
  configurable initial cardinality, sequential scale-up and secondary-only
  scale-down in its supported scenarios. Current Kuberic fixes target/minimum
  at 3/2 and freezes the original three nodes. Completion means explicitly
  supported initial sizes and resize operations with native build,
  configuration, quorum and retirement rules. Direct Primary removal is not
  implied.
  Evidence: [archived sizing/controller path](../archive/docs/features/kuberic/in-process-testing.md#scenario-ownership),
  [archived SQLite lifecycle scope](../archive/docs/features/sqlite/design.md#lifecycle-and-reconfiguration),
  [current CRD restriction](../kuberic-operator/src/crd.rs#L45-L47).

- [ ] **GAP-06: Durable application integrations.** Archived Kuberic included
  durable KVStore2, SQLite WAL replication and a PostgreSQL-native custom
  replicator. These applications have not been ported into the active stack;
  current deployed journeys use the fube sample package. Completion is
  application-specific: supported hosting/package integration, application-owned
  commit and publication, copy/catch-up, lifecycle and storage behavior.
  PostgreSQL integration must preserve its ownership of native WAL,
  synchronous policy and promotion. Source availability under `archive/` is
  not active support, and archive data-format migration is not implied.
  Evidence: [archived applications](../archive/README.md#applications),
  [archived PostgreSQL boundary](../archive/docs/features/postgres/design.md#architecture-and-authority),
  [current package contract](../kuberic-operator/README.md#build-and-install).

## Operational and developer capabilities

- [ ] **GAP-07: Authenticated control transport.** Archived agent RPCs required
  bearer credentials, without TLS. Current control TCP is unauthenticated and
  unencrypted and requires a trusted isolated network. Completion means an
  explicit authenticated control-channel contract and credential lifecycle
  integrated with native fube ownership. TLS would be additional hardening,
  not restoration of archived TLS support.
  Evidence: [archived transport contract](../archive/kuberic-runtime/README.md#public-v1-protocol),
  [current network limits](../kuberic-operator/README.md).

- [ ] **GAP-08: Downstream application test harness.** Archived Kuberic exposed
  an opt-in application-factory/in-process controller harness for downstream
  consumers. Current Kuberic has shared repository fixtures, mock API tests,
  actual-process tests and KinD journeys, but that downstream application
  integration has not been ported. Completion means a reusable supported
  application adapter with owned lifecycle/fault controls that drives the
  production controller/native path without exporting authority-mutation
  shortcuts. Existing fube facilities should be reused where appropriate.
  Evidence: [archived downstream harness](../archive/docs/features/kuberic/in-process-testing.md),
  [current fixtures](../kuberic-tests/src/lib.rs),
  [current validation scope](../README.md#build-and-basic-tests).

## Already present or not parity gaps

- Bootstrap, real copy/build and native admission, Primary/Secondary
  application-crash recovery on original Pods, repeated/reverse planned
  handoff, post-repair handoff and owned teardown already exist in the rebuild.
- Candidate-redirection handling exists. Genuine responsive unequal-history
  RA-triggered qualification remains separately deferred; it is not counted
  as a missing feature or a prerequisite for this backlog.
- Automatic rolling upgrades, mixed-version operation, production-ready
  distribution and migration from unsupported prior formats were not delivered
  by the archived stack either. They are separate future work, not lost
  capabilities. See the [archived unsupported operations](../archive/docs/features/kuberic/level-triggered-operator.md#unsupported-operations).
- General SF placement/policy parity, application spare-node pools and
  destructive data-loss recovery are not implied by restoring feature parity.

## Suggested sequencing and implementation boundaries

Start with **GAP-01**, restart-safe control ownership and recovery. Then scope
native reconnect/replacement work in **GAP-03/GAP-04** and persisted service
support in **GAP-02**. Durable application ports depend on their required
storage and lifecycle contracts; authentication and downstream harness work
can be scoped independently. This is a suggested order, not an authorization
to implement all items together.

Preserve the fube/SF ownership split: FM authorizes configuration and epochs;
RA evaluates replica histories and native quorum safety; RA/RAP/Hosting execute
replica lifecycle work. Identify missing upstream fube contracts per item
rather than reintroducing the archived runtime alongside fube. Do not change
the protected `Replicator`, `PrimaryReplicator` or `StateProvider` method sets,
add application spare pools, or weaken quorum rules to close these gaps.
