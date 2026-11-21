# Operator restart: source research and alternatives

**Background, not the implementation contract.** This preserves the source
research, current-implementation evidence and alternatives considered for the
[operator restart design](../operator-restart-design.md). The design document
defines the chosen minimum-state, watch-driven approach; the external systems
described here are not architectures Kuberic must reproduce.

## Service Fabric FM state and recovery

The following paths were read directly from the clean local checkout at
`/data/code/reference/service-fabric`, revision
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`. Source links below are pinned to
that revision. These are source observations, not results from running native
SF.

### Ordinary FM is itself a replicated stateful service

`FailoverManagerService` derives from `KeyValueStoreReplica`. Its factory
initializes a system-service replicated store, supplying replicator settings,
working/store directories, shared-log settings and legacy local-store settings.
`KeyValueStoreReplica::InitializeForSystemServices` selects the V1 replicated
store or V2/TStore path according to configuration.

When that service replica becomes Primary, the factory creates an ordinary FM
using its `IReplicatedStore` and opens it. On loss of the Primary role, it closes
and releases that FM instance. FM's in-memory caches and running tasks are
therefore not the sole durable authority.

Sources: [service base][sf-service], [store initialization][sf-factory-init],
[Primary role changes][sf-factory], [store backend selection][sf-backend].

### What FM stores

`FailoverManagerStore` is a typed transactional wrapper over `IReplicatedStore`.
Its record types include Nodes, failover units, applications, service types,
services, load, in-build failover units, upgrade/version metadata and string
key/value metadata.

For the Kuberic recovery problem, the important persisted facts are:

| SF record | Relevant contents |
|---|---|
| Failover unit | Partition descriptor, service name, flags, replicas, update/lookup versions, reconfiguration timestamps and other metadata |
| Partition descriptor | Partition/consistency-unit identity, PC/CC epochs, target/minimum sizes |
| Replica | Replica description, flags, version and lifecycle timestamps |
| Node | Exact Node instance, up/down state, replica-upload state, activation/deactivation and other Node metadata |
| Key/value metadata | Generation and lookup-version values loaded during FM initialization |

This is not a serialized thread, socket or message queue. Application bytes
remain owned by application replicas, not FM's store.

Sources: [store types][sf-store-types], [failover-unit fields][sf-ft-fields],
[partition descriptor][sf-ft-description], [replica fields][sf-replica-fields],
[Node fields][sf-node-fields], [cache initialization][sf-load].

### Commit ordering and optimistic concurrency

Store updates create transactions, serialize typed records and use each
record's operation LSN as the expected sequence for update/delete. Successful
commit calls `PostCommit`; a failed transaction is not adopted as committed
state.

For the ordinary persisted failover-unit path, FM retains generated
state-machine actions alongside the update. The commit callback enqueues a
commit job; only successful post-update processing invokes the actions.
`BackgroundManager` also supports actions derived from already committed state
when no new persistence is needed.

This is not a universal claim that SF fsyncs before every outbound message.
There is an explicit deferred-persistence optimization, including a guarded
stateless build-completion path. The Kuberic proposal does not port that
optimization: new authority and otherwise irrecoverable boundary evidence
must be committed before dependent effects. Re-queryable progress need not
become a durable record.

Sources: [transaction/update path][sf-store-update],
[serialization and expected LSN][sf-store-cas],
[async FT persistence][sf-ft-commit], [commit-action gate][sf-commit-job],
[background action path][sf-background],
[stateless lazy-persistence condition][sf-lazy].

### Normal recovery reloads state and regenerates work

FM initialization enumerates persisted record types, recreates caches and
pointer relationships, restores generation/lookup metadata, and starts
background processing after load completion. `PendingTask` derives work such
as `DoReconfiguration`, replica creation/build and deletion from current
partition/replica state.

The useful model is **durable semantic state plus repeatable effects**, not a
durable copy of every original transport envelope. Losing an action queue
does not mean its committed intent disappeared.

Sources: [load and cache reconstruction][sf-load],
[readiness/background startup][sf-ready], [pending action regeneration][sf-pending].

### Native FM retry and timeout semantics

Follow-up local source inspection resolved the review's timeout-policy
question in favor of matching native FM:

- `PendingTask` regenerates eligible create/build, reconfiguration and deletion
  actions from current FT/replica state. A single lost action/reply does not
  erase the persisted intent.
- `FailoverUnit::CanGenerateAction` and `Replica::CanGenerateAction` pace
  retries. `Replica::GenerateAction` updates `lastActionTime_`, but that retry
  clock is not included in the replica's serialized fields.
- `FailoverUnit::GetCurrentHealth` classifies an over-limit reconfiguration as
  `ReconfigurationStuck`. This is health reporting, not a blanket fatal
  completion deadline.
- The reconfiguration reply processor accepts successful current
  reconfiguration results and completes the FT transition. It does not impose
  a separate Kubernetes/durable-fold deadline on that result.
- Actual native errors have action-specific consequences. For example, failed
  AddPrimary/AddReplica replies can mark/drop the replica; deletion is
  completed only on success. "Retry pending actions" does not mean ignore
  failed builds or invent success.

Sources: [pending actions][sf-pending], [FT pacing][sf-action-pacing],
[replica pacing][sf-replica-pacing], [serialized fields and local retry clock][sf-retry-not-stored],
[stuck health][sf-reconfiguration-health], [reconfiguration replies][sf-reconfiguration-reply],
[build reply errors][sf-build-reply], [deletion replies][sf-delete-reply].

SF has other explicit timing policies, including configured quorum-loss
behavior which can drop offline replicas. Those are different state-machine
policies, not a generic terminal FM request timer. They do not authorize
Kuberic to waive native quorum or introduce data-loss behavior in its fixed
volatile profile.

Source: [separate quorum-loss policy][sf-timed-policies].

Accordingly, the chosen Kuberic contract now keeps retry pacing and attempt
timeouts local, preserves C/T across uncertainty, and accepts actual valid
current-intent completion after a controller/API delay. It removes the
previously proposed persisted command-deadline/error-latch machinery.
This intentionally changes the experimental process-only FM's fatal session
timeout policy; it does not change current runtime code or disable native
RA/RAP, RPC or process-shutdown timeouts.

### Normal FM recovery and full rebuild are different

At this revision, initialization enters `RebuildContext` for FMM, or for an
ordinary FM whose failover-unit cache is empty under the configured condition.
A normal nonempty ordinary FM store takes the load-completion path instead.

Rebuild uses generation proposal/update, correlated LFUM inventory uploads and
in-build failover-unit reconciliation. `SetGeneration` updates the in-memory
generation only after its store write succeeds. Rebuild upload processing
validates the active generation and expected Node incarnation, processes the
reported inventory/Node state, and suppresses the reply if processing fails.

Initial `NodeUp`/`ReplicaUp` discovery is another distinct protocol. Native RA
performs per-partition `NodeUpAck` work and pending-replica upload processing.
FM persists successful final replica-upload state before acknowledging that
the last upload completed. An empty `ReplicaUp` cannot stand in for discovery
of live replicas.

Sources: [load-versus-rebuild decision][sf-load],
[generation persistence][sf-generation], [rebuild protocol][sf-rebuild-start],
[LFUM processing][sf-rebuild-upload], [NodeUp handling][sf-node-up],
[RA discovery][sf-node-up-ack], [final ReplicaUp acknowledgement][sf-replica-up].

FMM is also not ordinary FM's durable-store implementation:
`CreateFMM` uses `FauxLocalStore`, while ordinary FM receives its replicated
service store. Kuberic should not emulate FMM's bootstrap/rebuild arrangement
merely because it implements an external FM.

Source: [ordinary FM versus FMM construction][sf-fmm].

### Generation fencing is not application-writer fencing

RA rejects ordinary messages below its receive generation. Generation proposal,
receive-generation advancement and send-generation advancement are separate.
A matching LFUM reply advances send generation; it does not finish RAP
reconfiguration, grant a replica role, or prove that an old application writer
stopped.

SF also has replicated-service Primary ownership and Federation infrastructure.
Copying a `GenerationHeader` check into Kuberic is not a replacement for that
control-owner authority.

Sources: [receive lower-bound check][sf-generation-check],
[proposal/update/send rules][sf-generation-rules], [FM Primary lifecycle][sf-factory].

## CNPG compact intent and reconstructed progress

The clean local checkout at `/data/code/reference/cloudnative-pg`, revision
`8f824a4aa60050ab0b85c48fdb4d32452ef4e3db`, was inspected directly. Its source
does not support the claim that CNPG stores only two strings: `ClusterStatus`
contains many resource summaries, timestamps, conditions and integration
fields. The useful distinction is **durable control intent versus observed
progress**, not the total number of status fields.

| CNPG mechanism | What the local code does | Lesson for Kuberic |
|---|---|---|
| Target/current Primary | Controller/tooling writes `targetPrimary`; the target instance checks its real database role, promotes if needed, then writes `currentPrimary` | Retain accepted desired configuration separately from native-completed configuration |
| Promotion resumption | An already promoted database does not need another promotion merely because its status update was lost | Re-observe or replay native completion instead of persisting callback-by-callback progress |
| Failover marker | A pending-failover target marks intent before selecting a successor after WAL receivers stop | A small semantic transition marker can be necessary; a generic workflow journal is not |
| Resource/status refresh | Reconcile lists owned resources and queries instance status; counters, topology and reports are refreshed | Reconstruct Pod/Service facts and native progress; do not make a second durable copy authoritative |
| Watch/requeue | The operator watches Cluster and children; instance managers also watch Cluster; waiting paths requeue | A process restart re-enters normal reconciliation, not a bespoke state-machine playback loop |
| Timing | Target/current timestamps and the primary-failing-since timestamp survive controller loops | Retain a time fact only where resetting it would change accepted behavior |
| Status writes | Phase updates/tooling use an optimistic-lock helper that refreshes and retries conflicts; the target setter also uses a merge patch | Use deliberate CAS for Kuberic authority; do not assume every CNPG status patch is a transaction |

Sources: [status fields][cnpg-status], [timestamps][cnpg-timestamps],
[target update][cnpg-target], [instance promotion/completion][cnpg-primary],
[failover intent][cnpg-failover], [resource observation][cnpg-resources],
[observation summaries][cnpg-observations], [instance status query][cnpg-query],
[operator watches][cnpg-watch], [instance watch][cnpg-instance-watch],
[wait/requeue][cnpg-wait], [failover timing][cnpg-time],
[optimistic status helper][cnpg-patch].

The current source even marks `latestGeneratedNode` deprecated/not set and
derives an available instance name from observed names, checking terminating
PVCs before reuse. Do not copy an outdated claim that CNPG always persists a
monotonic name counter. Kuberic's native replica incarnation fence is a
different requirement and cannot be replaced with name reuse.

Sources: [deprecated field][cnpg-status],
[name derivation][cnpg-names], [terminating-PVC guard][cnpg-name-reuse].

CNPG also has safety state and actors outside its Cluster object:
the inspected revision acquires a per-cluster primary Lease before promotion
and has a separate `FailoverQuorum` resource for synchronous-election evidence.
Its instance managers and PostgreSQL storage retain database state; the
operator does not reconstruct WAL from Cluster status. The primary Lease
runnable distinguishes preemption from transient API failure and leaves
network-isolation fencing to other instance machinery.

Sources: [primary acquisition][cnpg-primary], [primary Lease resource][cnpg-lease],
[Lease behavior][cnpg-lease-runtime], [failover quorum][cnpg-quorum].

Borrow the reconciliation architecture, not PostgreSQL's role/election rules.
For example, CNPG can reset an absent target from live Pod observations; that
does not authorize Kuberic to abandon an already-issued SF configuration.
CNPG phase fields also participate in some guards, so "all phase fields are
purely cosmetic" is not a conclusion of this research.

Source: [target reconciliation][cnpg-observations].

## Current Kuberic implementation evidence

This describes the pre-recovery implementation, not the proposed durable
schema. The existing KubericSet is an appropriate place for minimum authority,
but its fields and write ordering are not yet a safe recovery contract:

- [CRD status](../../kuberic-operator/src/crd.rs#L208-L233) contains allocation,
  bindings and observed phase/endpoint/configuration/switchover information.
  Active membership, fresh repair incarnations, pending commands and replay
  fences also live in [FM memory](../../kuberic-fm/src/state.rs#L129-L154).
- The [runtime sends effects](../../kuberic-fm/src/runtime.rs#L125-L134) before
  publishing its status observation. An observed status write is not a
  pre-effect transaction.
- [Backend admission](../../kuberic-operator/src/admission.rs) and
  [cleanup-report caching](../../kuberic-operator/src/processes.rs) are retained
  by the original backend. Recreating a reconciler around the same backend is
  not recovery after losing the backend process.
- [Operator allocation](../../kuberic-operator/src/runtime.rs#L82-L154)
  currently reserves a concrete operator Pod-IP listener and creates fresh FM
  authority. Those endpoint/identity assumptions do not survive arbitrary Pod
  replacement.
- The [FM adapter](../../kuberic-fm/src/runtime.rs#L153-L178) rejects reconnect.
  Fube's [failure contract](../../fube/docs/cluster/ExternalFmContainerNode.md#failure-behavior-and-limits)
  makes original FM-channel loss control uncertainty, not rejoin permission.

The initial research used fube
`117b81346bef0cc55fa6287b9f81f0abd41f0046`. A subsequent dependency update to
`38dc0578636d62eb5eb7b6f4f3647d544600fe39` added live-session LFUM overlap
qualification and extracted the local cluster's
[`fube-fm` process](../../fube/crates/tools/fube-cluster/README.md#fm-process-boundary).
Its manager owns in-memory FM state/native Channels; the local supervisor
retains Node processes and physical cleanup. The process uses private same-host
management IPC and source-bound launch metadata/receipts, not Kuberic's
external-FM listener contract. It adds no reconnect, durable FM recovery or
FM rebuild.

The later pin `d8a56a99a2b3c6f38875577af9737413f6568e89` replaces active
transport paths with `furpc` and retires `fube-transport`. Kuberic migrates its
FM/session adapter and scripted fixtures to that shared API. Replay equality
compares ordered header records and visible body bytes rather than body segment
boundaries; correlated native replies remain owned by the original session.
Relative fube references resolve to this latest checkout. The R1-R6 cross-session
requirements remain unimplemented gates.

## Alternatives not selected

The selected approach is minimum accepted authority on KubericSet, with
progress reconstructed by the control loop. The following alternatives are
recorded as rationale, not additional implementation work:

| Alternative | Disposition |
|---|---|
| Recreate `ProcessFm` from current public status | Rejected: missing durable semantic state, commit ordering and native reconnect |
| Serialize the complete Rust FM/backend object | Rejected: process/session handles and relative time are not durable protocol authority |
| Separate FM-state CRD or full operation journal | Not selected by default: adds authority duplication and lifecycle machinery without proving the extra state necessary |
| Keep FM in a separate long-lived service | Can isolate Kubernetes watcher restarts, but leaves FM failure/recovery unsolved; not required for this design |
| Deploy an SF-style replicated FM database | Unnecessary for this bounded Kubernetes deployment; use Kubernetes durability/availability instead |
| Rebuild authority from Nodes on every restart | Rejected: normal persisted recovery is not lost-store rebuild, and inventory is not final reconfiguration authority |
| Permanently fail an accepted configuration because its completion write missed a controller deadline | Rejected after checking native FM: retain intent, retry only eligible native work and validate the actual outcome; do not make metadata API latency a new authority boundary |

Kubernetes CAS replaces the replicated-store record transaction for one
partition, not SF's entire storage/availability architecture. Kuberic does not
inherit Federation, FM service-quorum availability, unrestricted multi-record
transactions or SF's complete rebuild behavior. Its application quorum remains
the native replica quorum, independent of the Kubernetes metadata store.

## Design review disposition

The proposed restart/state contracts received an independent Astra review and
a separate parent-model simplification pass (GPT-6.1 Sol). Astra found no P1
or high-confidence configuration/writer-safety defect in the conditional C/T
model, but identified the same teardown admission gap as the parent pass.
Its narrow follow-up confirmed the cleanup-only correction and required
closure obligations. Subsequent local SF verification and the user's request
to match native behavior supersede the initially retained fatal-deadline
policy. This is design/source review, not runtime qualification or proof that
fube implements the required contracts.

| Finding or simplification | Disposition |
|---|---|
| P2: generic recovery requires reconnecting to correctly retired Nodes | Fixed in the proposal: complete native deletion receipts select [cleanup-only recovery](../operator-restart-design.md#cleanup-only-recovery) before FM/listener construction or native admission |
| Pending physical reports after native deletion | R5 now explicitly requires safe original Node/supervisor closure after FM transport loss, without new FM ACKs; exact cleanup receipts and child UID/resourceVersion fencing remain mandatory |
| Takeover while old FM TCP remains open | R1 now has an explicit candidate-session/generation handover sequence; EndpointSlice repointing and old TCP closure are not admission proofs. The actual upstream path remains gated |
| Per-command deadline and persistent hold machinery | Removed from the chosen persisted schema after checking native retry/health paths; clocks remain local/diagnostic, not new configuration authority |
| Reconcile proven current-T completion after a deadline | Selected to match native FM: preserve C/T, verify current ownership and actual native success, then CAS completion. This intentionally supersedes the earlier process-only fatal timeout policy |
| Separate allocation/redirection counters and copied role arrays | Still unnecessary under the documented no-abandonment/fixed-profile proofs; retain the derived values |
| Separate FM-state CRD, durable event queue or copied RA phases | Still unnecessary; retain one KubericSet authority and observation-driven progress |
| Shared multiplexing listener instead of per-set Service/EndpointSlice | Not an obvious simplification: it adds a native identity-dispatch contract; retain the bounded per-set transport proposal |
| Omit the R4 wire-description reconstruction contract | Rejected: existing exact same-Primary checks and native SF's description-based message construction do not justify arbitrary LSN/endpoint normalization |

The last point was checked against native
[configuration-message construction][sf-create-config] and
[explicit LSN reset][sf-reset-lsn]. A native-approved semantic reconstruction
recipe remains an allowed solution; historical transport envelopes need not
be persisted. Do not silently drop the reconstruction obligation to make the
stored schema smaller.

### Structural review follow-up

A subsequent read-only GPT-6 Astra structural review found no blocking design
defect conditional on the native contracts. Its recommendations are selected
in the active design, not implemented runtime changes:

| Recommendation | Disposition |
|---|---|
| Avoid retaining two cooperating FM decision loops | Specify one pure FM decision core, a sole reconciler authority writer and separate effect adapters; replace the autonomous Kubernetes `ProcessFm` path rather than adding persistence callbacks |
| Make bootstrap prerequisites precede all-Node admission | Derive provisioning/native-control/cleanup-only modes without a saved cursor; finish missing original configuration publication before requiring all Nodes, without interpreting absent C as no prior effects |
| Define the initially unbound discovery Service | Permit only an absent UID/address pair, then bind both atomically before original configuration publication; bound identities remain immutable |
| Remove hidden observation mutations and local admission authority | Require read-only evidence observers and explicit fenced report-retention effects; parent receipt CAS remains exclusively in the reconciler |
| Validate scattered authority once | Introduce an in-memory `AuthorityView` per parent revision, not another stored authority or native-readiness cache |

These are component and ordering refinements, not a claim that the archived
operator lacked level-triggered reconciliation. No extra journal, persisted
stage, field-elimination target or native capability is introduced. Exact
completion, R1-R6, positive cleanup and fixed-profile scope remain unchanged.

## Source references

All SF and CNPG references below were checked against the respective local
revisions recorded above.

[sf-service]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/Service.cpp#L15-L24
[sf-factory-init]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/ServiceFactory.cpp#L76-L120
[sf-factory]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/ServiceFactory.cpp#L188-L255
[sf-backend]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Store/KeyValueStoreReplica.cpp#L397-L504
[sf-store-types]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/Store.h#L88-L103
[sf-ft-fields]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverUnit.h#L279-L292
[sf-ft-description]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/common/FailoverUnitDescription.h#L29-L75
[sf-replica-fields]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/Replica.h#L215
[sf-node-fields]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/NodeInfo.h#L41-L58
[sf-store-update]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/Store.cpp#L200-L307
[sf-store-cas]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/Store.cpp#L784-L927
[sf-ft-commit]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverUnitCache.cpp#L525-L555
[sf-commit-job]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/CommitJobQueue.cpp#L62-L115
[sf-background]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/BackgroundManager.cpp#L623-L720
[sf-lazy]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverUnitMessageProcessor.cpp#L92-L114
[sf-load]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverManager.cpp#L215-L438
[sf-ready]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverManager.cpp#L626-L640
[sf-pending]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/PendingTask.cpp#L19-L198
[sf-generation]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverManager.cpp#L777-L793
[sf-rebuild-start]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/RebuildContext.cpp#L44-L90
[sf-rebuild-upload]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/RebuildContext.cpp#L389-L486
[sf-node-up]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverManager.cpp#L1739-L1829
[sf-node-up-ack]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/NodeUpAckProcessor.cpp#L100-L154
[sf-replica-up]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/ReplicasUpdateOperation.cpp#L337-L421
[sf-fmm]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverManager.cpp#L51-L90
[sf-generation-check]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/GenerationStateManager.cpp#L23-L40
[sf-generation-rules]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/GenerationStateManager.cpp#L245-L355
[cnpg-status]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/api/v1/cluster_types.go#L895-L1005
[cnpg-timestamps]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/api/v1/cluster_types.go#L1059-L1072
[cnpg-target]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/controller/cluster_status.go#L752-L761
[cnpg-primary]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/management/controller/instance_controller.go#L1200-L1312
[cnpg-failover]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/controller/replicas.go#L120-L196
[cnpg-resources]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/controller/cluster_status.go#L137-L169
[cnpg-observations]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/controller/cluster_status.go#L244-L324
[cnpg-query]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/controller/cluster_controller.go#L454-L485
[cnpg-watch]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/controller/cluster_controller.go#L1317-L1349
[cnpg-instance-watch]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/cmd/manager/instance/run/cmd.go#L261-L280
[cnpg-wait]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/controller/cluster_controller.go#L408-L427
[cnpg-time]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/controller/replicas.go#L433-L461
[cnpg-patch]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/pkg/resources/status/patch.go#L34-L80
[cnpg-names]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/controller/cluster_create.go#L1123-L1136
[cnpg-name-reuse]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/controller/cluster_create.go#L1417-L1446
[cnpg-lease]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/controller/primary_lease.go#L48-L102
[cnpg-lease-runtime]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/internal/cmd/manager/instance/run/lease/runnable.go#L423-L533
[cnpg-quorum]: https://github.com/cloudnative-pg/cloudnative-pg/blob/8f824a4aa60050ab0b85c48fdb4d32452ef4e3db/api/v1/failoverquorum_types.go#L43-L78
[sf-create-config]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverUnit.cpp#L1524-L1548
[sf-reset-lsn]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverUnit.cpp#L1605-L1612
[sf-action-pacing]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverUnit.cpp#L1560-L1577
[sf-replica-pacing]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/Replica.cpp#L661-L667
[sf-retry-not-stored]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/Replica.h#L215-L250
[sf-reconfiguration-health]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverUnit.cpp#L1853-L1886
[sf-reconfiguration-reply]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverUnitMessageProcessor.cpp#L413-L527
[sf-build-reply]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverUnitMessageProcessor.cpp#L46-L90
[sf-delete-reply]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/FailoverUnitMessageProcessor.cpp#L224-L254
[sf-timed-policies]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/fm/ReconfigurationTask.cpp#L109-L131
