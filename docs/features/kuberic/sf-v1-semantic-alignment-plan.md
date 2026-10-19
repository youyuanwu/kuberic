# Faithful SF V1 Replicator Port: Design and Implementation Plan

## Status and Compatibility Target

**Roadmap, updated 2026-10-19. No selected runtime behavior changes.**
The separately delivered [P1 foundation ledger](sf-v1-port-ledger.md) and
[provenance/notices](sf-v1-port-provenance.md) record source-derived scenarios
and protected-type guards, not ported engine behavior or complete alignment.
Scenarios remain documented acceptance criteria until tests can exercise the
actual ported components; checking a locally redefined formula is not behavior
coverage.
The [OperationQueue slice](sf-v1-port-ledger.md#operationqueue-slice-p2) now
provides an internal, unselected source-guided queue with 42 original component
tests. The [bounded sender/session/ACK slice](sf-v1-port-ledger.md#sender-session-ack-slice-p2-bounded-f06)
adds private reliable scheduling, ACK batching and selected session coordination
with 61 actual component tests. It is not a full
RemoteSession/ReplicationSession port: copy/build/role owners and session
slow-fault/MustCatchup/promotion integration remain explicit. The private
[async/tonic substrate](sf-v1-port-ledger.md#async-tonic-substrate-slice-p2-bounded-f05f08f11)
adds the logical carrier, explicit lossless buffer adapter and timer ownership,
with 54 additional actual tests (157 combined v1 tests).
The [bounded primary queue/quorum slice](sf-v1-port-ledger.md#primary-queue-and-quorum-slice-p3-bounded-f02f06f09)
adds private primary assignment/completion, configuration progress and catch-up
ownership on top of those components, with 44 actual tests (201 combined v1).
It is not the full primary/configuration
lifecycle: slow-fault, promotion, receiver/copy/provider/role integration and
all activation/hosted-parity gates remain pending.
The [bounded secondary receiver/ACK slice](sf-v1-port-ledger.md#secondary-receiver-and-ack-slice-p3-bounded-f04f05f06f09)
adds private ordered dispatch, required/optimistic service ACK ownership,
shared replication streams and explicit epoch barriers, with 92 actual tests
(293 combined v1). Provider invocation, native fault/report/drain aftermath,
copy/context/build and full secondary/role integration remain pending.
The [bounded copy-receiver slice](sf-v1-port-ledger.md#copy-receivers-slice-p3-bounded-f04f05f07)
adds secondary copy receipt and primary-side receipt of target context, reusing
the queue/callback/shared-stream/ACK foundations, with 68 additional actual
tests (361 combined v1). Copy/context enumeration and
sending, StartCopy establishment, build gates and cross-receiver provider/
fault/drain/role orchestration remain pending; these receivers do not execute
those owners or activate the replacement.
The [copy-enumeration slice](sf-v1-port-ledger.md#copy-enumeration-slice-p3-bounded-f05f07f11)
adds serial copy/context stream polling and reliable sending (45 actual tests,
406 combined V1). The [CopySender build-gates slice](sf-v1-port-ledger.md#copysender-build-gates-slice-p3-bounded-f06f07f11)
adds final-copy/replication completion, actual EOF-issued target capture,
pending-target retarget and disabled-completion behavior (47 actual tests,
453 combined V1). The [copy-establishment slice](sf-v1-port-ledger.md#copy-establishment-slice-p3-bounded-f04f05f07f08f11)
adds reliable StartCopy, bound identity/epoch admission and protected target
GetCopyContext/source GetCopyState acquisition using the existing components
(45 actual tests, 498 combined V1). The [BuildIdle lifecycle slice](sf-v1-port-ledger.md#buildidle-lifecycle-slice-p3-bounded-f02f05f06f07f11)
adds idle-session admission, classic frozen-boundary/pending replication capture,
ordered build orchestration and removal/cancellation/completion cleanup
(41 actual tests, 539 combined V1). The [secondary lifecycle slice](sf-v1-port-ledger.md#secondary-lifecycle-slice-p3-bounded-f04f05f06f09f11)
adds one shared receiver/establishment/context/ACK lifetime, actual provider
epoch callbacks, cross-receiver faults and ordinary legacy/EOS Close/drain/Abort
(41 actual tests, 580 combined V1). The [primary lifecycle slice](sf-v1-port-ledger.md#primary-lifecycle-slice-p3-bounded-f02f06f07f08f09f11)
adds retained build/session lifetime, post-build retry/RequestAck timers,
bound role-aware ordered ACK/quorum delivery and ordinary quorum prewait/
Close/Abort effect drainage (42 actual tests, 622 combined V1).
The [primary configuration slice](sf-v1-port-ledger.md#primary-configuration-slice-p3-bounded-f02f06f08f09)
adds descriptor-based catch-up/current updates, actual session creation/reuse,
idle-to-active session marking/average reset and existing replay/retirement/
quorum/catchup composition (34 actual tests, 656 combined V1). Configuration
reuse is ID-based while bound ACK identity remains strict.
The [role-transition slice](sf-v1-port-ledger.md#role-transition-slice-p3-bounded-f01f02f04f09f11)
adds native role edges, ACK-mode-specific promotion drainage, real retained
queue/original-epoch handoff, demotion limits and external primary/secondary
epoch sequencing with actual-component regressions. These supersede preceding
slice-boundary omissions.
The [data-loss recovery slice](sf-v1-port-ledger.md#data-loss-recovery-slice-p3-bounded-f01f02f03f09f11)
adds primary-only checking, changed/unchanged provider recovery, actual queue
reset, retained callbacks and native error/state-restoration ordering.
The [queue-policy slice](sf-v1-port-ledger.md#queue-pressure-and-slow-peer-policy-slice-p3-bounded-f02f03f06f07f08f11)
adds periodic freshness and pressure retargeting, slow-peer completion-disable/
sender closure and reclamation exclusion, and retrying typed fault notifications.
Quorum/all-applied and live MustCatchup retain faulted active replicas.
The [message-ingress slice](sf-v1-port-ledger.md#role-aware-message-ingress-slice-p3-bounded-f01f04f05f08f09f11)
adds private role-aware typed dispatch, native secondary message/epoch admission,
current-primary reply routing and once-only targeted remote fault reporting.
The [Open/restart recovery slice](sf-v1-port-ledger.md#open-and-restart-recovery-slice-p3-bounded-f01f04f07f08f09f11)
adds explicit Created/Open and private logical ingress registration,
lazy normalized/cached provider progress and a caller-directed actual-owner
restart-to-copy/replication handoff without DurableState or invented history.
Broader production host recovery, automatic replica rebuild/placement policy,
host listener/authentication/registry and partition-report integration, and
monitoring/health remain pending.
The [private service-adapter slice](sf-v1-port-ledger.md#private-service-adapter-slice-p4-bounded-f01f02f04f05f09f11)
adds caller-driven resumable async control, separate assignment/quorum completion,
shared service streams/ACK/provider-barrier handoff and native progress queries
over actual owners. A helper-free in-memory service demonstrates separate
ordered commit, visibility and client completion. This bounded P4 preparation
does not implement or activate the published facade, real KV/SQLite migration,
host listener/authentication/registry/status/reporting or atomic cutover.
The [private protected-facade execution slice](sf-v1-port-ledger.md#private-protected-facade-execution-slice-p4-bounded-f01f02f09f11)
now implements unchanged protected traits over those owners with a retained
active/retired driver, independent build/catch-up waits, synchronous Abort
gating and separate Close/join lifetimes. Published selection, assignment/ACK
APIs and their current durability contract remain unchanged until P5.
This roadmap replaces isolated audit-item prioritization with a **faithful
source-guided port of Service Fabric classic V1**, implemented using Rust async
and tonic transport. This is the user's refined implementation direction,
not a request to keep patching the current algorithm until selected tests pass.

The target is observable service behavior, not merely similar interface names
or a more restrictive Kuberic contract. Match assignment/completion, application
ownership, ACK modes, streams, copy, progress, configuration, recovery, access,
faults and lifecycle outcomes. Preserve native algorithms, queue boundaries,
role transitions, session/ACK processing and configuration arithmetic; use
idiomatic Rust ownership and async execution without changing their effects.
COM ABI, native memory layout/wire interoperability, SF placement algorithms,
and V2 transactional replication are not the target.

An in-scope capability that is unsupported or unverified remains an explicit
compatibility blocker. The existing KSF catalog and audit closures do **not**
establish complete native parity. This plan does not close any audit finding.

### Source Baselines

- Native SF, verified clean:
  [`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`](https://github.com/microsoft/service-fabric/tree/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d).
- Kuberic current implementation:
  [`546269cb867dee1d53b4101987501e90df66e763`](https://github.com/youyuanwu/kuberic/tree/546269cb867dee1d53b4101987501e90df66e763),
  including the SF-10 fix.
- Native production classic V1 paths are primary evidence. Public managed
  documentation explains service obligations; production KVS is an example,
  not a universal provider durability guarantee. V2 internal marker behavior
  and binding comments are not substituted for the classic contract.

## Research: Current Versus Native Behavior

| Surface | Native classic V1 target | Current Kuberic |
|---|---|---|
| Assignment and completion | Successful queue admission exposes an LSN before separate quorum completion; failed enqueue does not consume the next LSN. | One serialized `replicate().await` returns after provider apply, quorum and local commit. [N1], [K1] |
| Primary persistence | Primary counts as one ACK; engine completion does not inspect its application commit. Service owns preparation, ordered commit, visibility and client response. | Built-in engine owns primary apply/commit through mandatory `DurableState`. [N1], [K1], [K2] |
| Secondary ACK | Persisted or `RequireServiceAck` requires explicit application ACK without a progress argument. Volatile default completes at ordered dispatch admission, potentially before dequeue. | Every operation requires explicit progress-bearing ACK and provider verification. [N2], [K3] |
| Copy context | Persisted target supplies context to source; volatile context exchange is omitted. | Source invokes its own provider context. [N3], [K4] |
| Copy stream | Copy EOF is terminal; sequential copy-then-replication and parallel consumers work. Getters share receiver state, not fresh replay. | Single-take replica-lifetime channels; `CopyComplete` is not EOF. [N3], [K3] |
| Build boundary | Copy starts from its admitted prefix; at copy enumeration EOF capture latest-issued replication LSN. Pending builds have explicit upward retargeting paths. | Frozen committed snapshot plus later committed catch-up C and exact durable receipt. Quiet prefix proof is fixed, not full native build parity. [N4], [K4] |
| Progress | Primary queue commitment; persisted secondary contiguous received cursor can precede service ACK; promotion inherits drained queue state. | Primary acknowledged durable committed cache; secondary service-ACKed durable acceptance; recovery reports also carry provider tails. [N5], [K5] |
| Catch-up capability | First retained committed/completed queue entry; zero when no such history, even with nonzero copied progress. | Committed tail, not retained floor (SF-D01). [N5], [K5] |
| Catch-up | All follows current latest-issued tail; WriteQuorum uses configuration-captured boundary plus moving must-catchup condition. | Readiness through local committed tail, with PC/CC checks or union membership. [N6], [K6] |
| Access/isolation | Lifecycle/quorum-based status, classic secondary reads denied; service owns transactional visibility. | Per-apply ambiguity/equality gates close reads, and ready ActiveSecondary reads are granted. [N7], [K7] |
| Close | Direct replicator Close can fail; outer host teardown aborts failed child and can complete successfully. | Outer program preserves child failure as failed Close after containment. [N8], [K8] |
| Epoch/recovery | Primary previous-epoch boundary is issued tail; secondary direct epoch and stream-induced provider barriers differ; changed data-loss state resets queue progress. | Uniform provider callback using reconciled commitment and same-epoch suppression; stronger retained-history recovery model. [N9], [K5] |
| Public capabilities | Persisted declaration, effective settings, ACK/EOS policies, health/load/move cost/fault reporting and restricted live settings updates. | Address-only settings, fault/load/status subset; no persisted/volatile declaration. [N10], [K9] |

Queue words are not storage promises. Secondary received, service-completed,
primary quorum-committed and provider locally committed boundaries must remain
distinct. A persisted service's ACK discipline supplies its durability guarantee;
volatile optimistic completion must never be reported as durable application
acceptance.

### Exact Native Catch-Up and Build Targets

Let `T` be current primary latest-issued LSN, `Qcc` current configuration quorum
apply-ACK progress, `Acc` all-current-replica apply progress, `B` the saved
configuration catch-up boundary, and `M` the minimum marked-successor apply LSN.
With enough supplied current replicas to satisfy the configured write quorum:

- **All:** `Acc >= T`, reevaluating T as writes arrive.
- **WriteQuorum:** `Qcc >= B`; if must-catchup is present, also `M >= Qcc`.
- B is raised/captured on configuration updates, not at each wait call.
  Ordinary subsequent writes do not continuously raise B, but can move Qcc/M.
- Previous/current configurations separately constrain normal queue commitment;
  the catch-up wait is not independently a fresh PC+CC quorum proof each time.
  Native special quorum-one and `AllowMultipleQuorumSet` behavior must be
  represented in the conformance ledger, not replaced with one generic majority.

Build is different again: its initial replication target is sampled at copy
enumeration EOF. While that replication gate is pending, periodic freshness can
raise it to current commitment, and queue pressure can raise it to latest-issued.
The inspected periodic default is 60 seconds; tests must drive time/pressure,
not sleep for a guessed duration. A quiet snapshot can seed the prefix without
the original operation record. [N4], [N6]

## Selected Component and API Design

### Faithful Port Boundary

The pinned native implementation is the algorithmic specification, including
error paths and less obvious state transitions. The current Kuberic engine is
the migration baseline and a source of safety regressions, **not** a second
specification to preserve when it conflicts with native behavior.

Port the production classic V1 engine under `Reliability/Replication`, with
explicit service/provider and host adapters. Do not embed SF, port its RA/FM/
federation/placement stack, or add a second permanently selectable engine.
The native dependency graph is translated by responsibility; a Rust file need
not correspond one-to-one with each C++ class, but no algorithmic responsibility
may disappear because tokio or gRPC looks reliable.

Proposed internal destination: `kuberic-runtime/src/replicator/v1/`. It is a
source organization for the replacement, not a new public factory or runtime
selection flag. `DefaultReplicatorFactory` switches once at P5; the old stateless
algorithm and its transport are removed at that cutover.

| Native component family | Rust port responsibility / proposed grouping | Required native behavior |
|---|---|---|
| `Replicator`, `ReplicatorState`, role async operations | `v1/replicator.rs`, `v1/state.rs` | Full initial/transitional/primary/secondary/faulted/closed transitions; role-based message admission and queue transfer, not one mutable role flag. |
| `PrimaryReplicator`, `ReplicaManager`, `ReplicationQueueManager` | `v1/primary.rs`, `v1/replica_manager.rs`, `v1/replication_queue_manager.rs` | CC/PC/idle sessions, assignment, completion map, captured/moving catch-up boundaries, native quorum special cases, build/recovery and reclamation. |
| `OperationQueue` | `v1/operation_queue.rs` | Ordered/gapped retention with distinct tail/commit/complete/history heads, count/byte limits, trimming, reset and transfer rules. A plain channel is not a substitute. |
| `SecondaryReplicator`, copy/replication receivers | `v1/secondary.rs`, `v1/receiver.rs` | Separate ordering/dispatch queues, persisted/volatile ACK modes, fixed copy versus minimum replication epochs, provider barriers and close/promotion drain. |
| `ReplicationSession`, `RemoteSession`, `ReliableOperationSender`, `AckSender` | `v1/session.rs`, `v1/sender.rs`, `v1/ack.rs` | Per-incarnation receive/apply cursors, ordered ACK processing, retry/window/request-ACK, batching/final ACK, slow-peer and backpressure policy. |
| `CopySender`, `CopyAsyncOperation`, `CopyContextReceiver`, establishment | `v1/copy_sender.rs`, `v1/copy_async_operation.rs`, `v1/copy_context_receiver.rs`, remaining full build/provider/stream adapters | Serial provider enumeration, one parked full-queue item, target context ordering/EOF, EOF-issued target, pending retarget and final-copy/replication gates; reliable StartCopy remains pending. |
| Native transport/demux/message headers | `v1/transport.rs`, replication proto | Tonic carrier with native logical message/session/epoch/LSN semantics; no data-operation RPC result used as service ACK. |
| COM/provider/operation/stream/settings facade | Existing public facade plus `v1` adapters | Preserve Rust protected types, early LSN/completion, lossless buffers, shared stream receivers, error classes, capabilities and effective defaults. |
| RA/partition boundary | Host/controller adapters, not engine internals | Native callback/access/Close-layer behavior expressed through Kuberic lifecycle authority; no provider commitment mislabelled as native queue progress. |

Every grouping requires a port-ledger row linking native source/pin, relevant
state transitions, Rust destination, allowed representation substitution,
native-derived test vectors and current implementation/review status. Native
headers/components and scenario sources are linked in [N12]-[N15]. No translated
component is considered complete from method signatures alone.

### Rust Async Execution and Tonic Transport

Use short, coherent queue/role/configuration state transitions; stage external
actions under that serialization and perform provider/RPC/service work outside
the state guard. No engine state lock is held across `.await`. Preserve native
immediate/reentrant completion ordering and exactly-once completion when replacing
Begin/End callbacks with owned futures/oneshots. A monolithic task awaiting a
provider or network call must not stall ACK processing or early assignment.

Tasks, timers, provider calls and completion callbacks retain their role/session
owner until the correct native drain/cancel boundary. Tokio task abortion or
dropping a future is not a replacement for native advisory cancellation or
application rollback. Inject clocks for ACK batching, retry, idle-build freshness
and slow-peer policy; preserve the pinned public defaults rather than inventing
new timing contracts.

Replace native one-way transport with **one tonic bidirectional exchange** on
the existing replication endpoint, carrying typed logical messages:
StartCopy, ReplicationOperation, CopyOperation, CopyContextOperation,
ReplicationAck, CopyContextAck, RequestAck and InduceFault. This is a proposed
new internal protobuf contract, not native wire compatibility. Remove obsolete
Replicate/Build RPCs during cutover rather than leaving a parallel data plane.

The envelope preserves full source/destination endpoint/session identity,
separate current sending-primary and original-operation epochs, operation
metadata/buffer sizes, replication/copy/context sequence spaces, terminal/error
markers and native reclamation hints. ACKs remain independently delivered
protocol events with distinct receive/apply cursors. Process replication
ACK/progress/build-gate updates before copy completion as native does.

HTTP/2 ordering/readiness does not eliminate native retransmission, deduplication,
gap handling, adaptive windows, RequestAck, per-message expiry or count/byte
backpressure. Bound tonic queues; a reconnect does not renew replica incarnation
or reset native progress. Preserve transient/full/too-large send outcomes.
Map native per-message expiration in the carrier/sender; do not replace it
with a gRPC deadline that cancels the entire session or an admitted operation.
Old streams cannot complete a new session, and stream failure is neither
implicit role change nor successful graceful drain.

Kuberic authority/history/storage IDs remain admission/fencing context, not
extra per-item durability or configuration-equality gates that reject valid
native session/epoch behavior during reconfiguration. This boundary must be
tested in normal native traces as well as stale-incarnation rejection. [N13]

### 1. Engine Queues, Service Data

The replicator owns admission/LSN ordering, retained **process-local** operation
queues, peer receive/apply cursors, quorum calculation, copy transport and
authority-scoped request lifetimes. It does not automatically mutate or commit
the primary application store.

The service owns validation, optional local preparation, ordered local commit,
rollback/isolation policy and client completion. Replication success alone
cannot generate a successful client write or expose an uncommitted service
value. KV and SQLite must migrate with the core ownership change, not after
access fences are removed.

`DurableState` becomes a service-selected persistence helper, not a mandatory
engine dependency or a second success oracle after native service ACK. Retained
engine queues supply retransmission/capability during a process lifetime;
provider state seeds recovery and copy after restart. Do not add an independent
replicator continuation database or another built-in engine.

**Selected:** faithfully port the native queue/manager/session/receiver algorithms,
then adapt only language/runtime/transport/host boundaries. A bespoke semantic
rewrite of the current engine is no longer selected. An early-number facade
over engine apply/commit still has conflicting owners; embedding the entire
SF runtime would replace deployment/hosting. Both alternatives are rejected.

### 2. Assignment Handle and Service ACK

Change the unprotected `StateReplicator` surface to expose an admission result/
assignment immediately and an independently awaitable replication completion.
The operation handle retains cancellation/completion ownership. Represent
unassigned/error outcomes explicitly; native internal zero on a failed admission
is not a successfully assigned application LSN. Assignment ordering and queue
capacity are checked before consuming the next sequence.

Several assigned operations may be pending concurrently. Services order their
local commits; they cannot infer order from callback arrival. Cancel is advisory,
not deletion of an admitted entry; dropping a waiter does not cancel replication
or promise rollback. Runtime-owned work remains observed and is retired by the
correct lifecycle boundary.

Service-facing `acknowledge()` reports application success with no Kuberic
progress argument. Internal receive, dispatch and contiguous completion counters
remain separate. Persisted services always require ACK; volatile services use the
declared required-ACK setting, default false. Implicit completion happens on
ordered dispatch admission, not after service dequeue.

Do not model a service's missing ACK as a native item-level negative ACK.
Application failure uses partition/stream fault semantics. Any convenience
`reject` adapter must fault/retire appropriately rather than inventing a
successful or rollback-shaped wire result. Late retired callbacks cannot advance
new sessions; match native no-op/error behavior by callback state, not a blanket
"every late ACK errors" rule.

### 3. Receiver-Lifetime Streams and Target Context

Copy/replication handles share the role receiver's queues; acquisition does not
rewind them. Copy reaches EOF after normal copy termination, independently of
replication shutdown. Model ordinary EOF and optional explicit EOS-ACK mode
separately. A temporarily empty open queue waits.

Idle-to-active preserves the receiver. Promotion drains/retire its secondary
according to ACK mode; demotion creates the proper secondary receiver with
already-completed empty copy. Duplicate Build is not permission to reopen
terminal streams. A new valid receiver/build lifetime must be established by the
native-compatible lifecycle; do not invent arbitrary in-place build generations.
This refines SF-18's earlier broad "later authorized build" test wording.

Obtain persisted context from the exact target and stream it to source
`get_copy_state`; omit context in volatile mode. Bound transport to the actual
source/target session and build lifetime. Handle empty enumeration, streaming
context, errors, cancellation and source/target replacement without source-local
or empty-success fallbacks.

### 4. Native Progress With Separate Provider Evidence

Use explicit engine observations: issued tail, received prefix, completed/apply
prefix, queue commitment, retained queue floor and lifecycle state. Implement
the role/query/NotReady rules, normalized initial provider no-data value,
promotion transfer and changed-data-loss reset from [N5], [N9]. Restore can reset
progress; monotonicity is scoped to the appropriate session/history.

The three protected interfaces keep their method sets and signatures.
Their current stricter Rustdoc is not immutable native semantics: reconcile
current-progress durability promises, previous-epoch boundary and snapshot
requirements against the native contract without adding provider hooks.
Early assignment/ACK changes belong to `StateReplicator`/stream/types/settings,
which are not protected interfaces.

**Change reports and all consumers atomically.** Today host reports use
`first=provider commitment,last=accepted/applied`, while controller descriptors
interpret first as capability and rejoin tests first==last. [K10]
Use named native current-progress/capability observations with query status,
separate from any provider-specific durable/recovery evidence. Do not redefine
the existing two integers in place. Update host projection, wire/validation,
agent persistence, controller/CRD, election, rejoin and recovery together;
version/reject incompatible state consistently with repository policy.
No successful LSN/capability pair alone grants access or proves client durability.

SF-10's quiet-copy invariant must remain. Its durable receipt/anchor is not a
new mandatory service persistence hook: translate it to correct receiver/session
copy-completion evidence when queue ownership changes. Preserve exact identity
and later-suffix checks without making an unavailable copied record a requirement.

### 5. Native Access, Epoch and Terminal Outcomes

Introduce a lifecycle/quorum access calculation rather than deriving serving
status from provider applied==committed. Stable classic secondaries return
NotPrimary for reads/writes; a primary may retain read access during demotion or
same-primary reconfiguration, and promotion has its own TryAgain/read-granted
stages. Replicate respects status/error class before internal role admission.

Only enable those status changes after service-owned read isolation and client
commit tests pass. Preserve authority/fault/retirement correctness fences, but
do not use them to disguise a normal native Granted state as NotPrimary merely
because one application operation is pending.

Match provider epoch callbacks and previous-issued-tail boundaries separately
from secondary receive-epoch invalidation and stream barriers. Distinguish direct
replicator Close failure from outer host teardown completion. Preserve child
diagnostics even when outer shutdown succeeds after Abort. Promotion drain,
ordinary secondary Close, optional EOS drain and direct Abort have different
conditions; no universal application-held-item ACK wait or forced-termination
promise is introduced.

### 6. Protected Data Types and Lossless Buffer Adapter

Interface protection includes resolved parameter/return types, not only method
spelling. Keep `OperationData = Bytes` and the protected
`OperationDataStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>`
unchanged. Do **not** redefine those aliases to a buffer vector.

Represent a native segmented operation through an optional service-side
buffer adapter: one stream item encodes one native operation as a versioned
frame containing buffer count, individual lengths and bytes. Decoding must
recover the exact ordered buffers, including zero buffers versus one empty
buffer, multiple empty buffers and arbitrary byte content. Provider copy/context
wrappers encode/decode through the existing protected byte streams; normal
replication/consumer companion types can expose a segmented view without
changing those protected types.

Select raw-single-buffer versus framed-segments representation explicitly for
the service/creation. Never sniff arbitrary application bytes for a magic prefix
or treat an empty byte item as stream EOF. Framing is a lossless Rust adapter,
not a new application ACK, persistence hook or snapshot record requirement.
Native operation/message limits account for the logical buffers separately
from adapter/transport overhead; corrupt frames fail, never become empty success.
Old/raw services retain their opaque single buffer. The engine must not rewrite
provider contents or require knowledge of their application encoding.

This uses the permitted Rust representation difference, not a protected API
exception. Pin expanded types with an external compilation fixture and test
copy/context/replication buffer round-trips before API activation. [K11], [N11]

### 7. Capability Completion, Not Silent Exclusions

Create an exhaustive ledger of the pinned classic engine's actually exposed
public interface/extension surface and its required host contracts.
Every entry is mapped to an implemented native-equivalent observation, a
representation-only difference, or an explicit pending compatibility blocker.
Optional interfaces are classified by production exposure, not IDL declaration
alone. Native ComReplicator exposes settings, stream faults/EOS and the
specific-quorum marker: port those behaviors. It does not expose the SDK's
atomic-group interfaces; those belong to separate ServiceGroup wrappers and
are not silently added to this engine port. Internal batch/reservation methods
that return E_NOTIMPL must not be advertised as implemented. The V2 internal
copy-until-latest marker and detailed platform-query plumbing stay separately
classified. This preserves native supported/unsupported boundaries. [N14]

Required work includes persisted-state declaration, required-service-ACK and
stream-fault/EOS policies; effective settings and native upgradeability/defaults;
queue/message limits and retention; pending-quorum-close behavior; listener/
published address handling; credential update/enforcement; and partition/
replica fault, health, load and move-cost behavior. Represent operation metadata/
data losslessly rather than erasing meaningful native buffer boundaries.

Reporting acceptance is not immediate cluster convergence. Kuberic may use its
Kubernetes control plane as the reporting destination, but must preserve scope,
validation, errors and required reporting behavior; an empty sink is not parity.
Credentials cannot be accepted without actual peer enforcement. No real secrets
belong in docs/tests.

### 8. Source Provenance and Notices

The native pin has root `LICENSE` (MIT) and `ThirdPartyNotices.txt`; engine
headers carry Microsoft MIT notices. Track derived source and translated native
test material in the port ledger. Preserve applicable upstream copyright,
permission and disclaimer notices for incorporated material, with a dedicated
`LICENSES/service-fabric-MIT.txt` and public port-provenance/notice document
when implementation begins; do not overwrite Kuberic's license or assume all
native dependencies are MIT. Review selected utilities/test dependencies against
their own notices. Prefer Rust runtime/transport equivalents to importing the
native Common/Transport/Boost/KTL dependency graph. No upstream implementation
or license text is copied by this planning-only change. [N15]

## Dependency-Ordered Implementation Program

The phases below remain incomplete except the separately delivered P1 foundation;
P2 has unselected OperationQueue, bounded sender/session/ACK and async/tonic
substrate slices; P3 has bounded primary queue/quorum, secondary receiver/ACK,
secondary-copy, primary-side copy-context receipt and copy/context stream
enumeration/reliable-send and CopySender build-gate/EOF-issued-target/retarget
ownership cores, with bounded provider/context acquisition and reliable StartCopy
identity/epoch establishment and bounded BuildIdle/idle-session admission,
classic boundary/pending-replication capture, ordered orchestration and
removal/cancellation/completion cleanup. Bounded secondary lifetime now adds
shared receiver/context/ACK ownership, provider epoch callbacks and ordinary
legacy/EOS Close/drain/Abort. Bounded primary lifetime now retains builds/session
timers/ordered ACKs and quorum prewait/Close/Abort effects. Descriptor-based
configuration creates/reuses sessions, marks idle-to-active session promotion and
composes replay/retirement with the actual manager. Bounded role orchestration
now adds promotion-specific drains, retained queue handoff/demotion and direct
external epoch sequencing. Bounded data-loss orchestration now composes actual
provider recovery, changed-only progress/reset and native admission/restoration.
Bounded queue policy now composes periodic/pressure retarget, slow-peer
exclusion and retrying notifications with native quorum/all/MustCatchup behavior.
Bounded private role-aware message dispatch and targeted remote fault reporting
now compose existing primary/secondary owners and real reply identity.
Private Created/Open, logical ingress registration and lazy normalized/cached
initial progress now compose an actual-owner caller-directed restart/copy/
replication journey. Broader production host recovery and automatic rebuild/
placement policy, production host
listener/authentication/registry and partition-report integration, and platform
monitoring are pending. P4 now has a bounded private service/control adapter:
resumable caller-driven lifecycle, early assignment/separate quorum completion,
shared receiver/ACK/provider-barrier adaptation and helper-free service ownership
evidence. A private protected-trait execution bridge now retains active and
retired work independently of callers, without changing public selection.
Published facade/assignment/ACK integration, real services and host/status/
reporting integration remain pending. This roadmap is not
authorization to implement remaining work. P2-P4 port and test source-mapped components before
production activation; they do not activate a partially migrated public contract.
Existing production factory wiring, protected types, required
storage, service writes, report meaning and status remain unchanged through P4.
P5 switches the entire dependent runtime/service/report/controller set atomically.
Preparatory commits compile and preserve existing supported journeys; no second
factory/engine, public compatibility switch, shadow data writer or success
fallback is introduced. Component tests are not counted as hosted native parity.

| Phase | Dependencies | Deliverable and owning surfaces | Observable exit gate |
|---|---|---|---|
| P1 Port ledger, notices and scenario oracle | None | Map native components/transitions/defaults/capabilities, resolved protected types, licensing and native test vectors. Extend fixtures without claiming native execution or pending parity. | Each source family and contract row has a destination, native evidence, allowed substitution and deterministic vector. Expanded protected signatures are pinned; supported/unsupported native capabilities remain explicit. |
| P2 Queue/session/async/tonic substrate | P1 | Port OperationQueue, ReliableOperationSender/AckSender and session delivery mechanics; implement bounded typed tonic carrier, deterministic clock and buffer adapter. Not production-selected. | Native-derived gap/limit/trim/transfer, receive/apply, retry/window/request-ACK, batching/expiry/backpressure, reconnect and buffer vectors pass with fake and real tonic carriers. Current production journeys stay unchanged. |
| P3 Native primary/secondary/copy state machines | P2 | Port ReplicaManager/ReplicationQueueManager, private Open/role coordinator, primary/secondary receivers, provider recovery/barriers and copy/context/build state machines. Not production-selected. | Source-mapped Open/restart recovery, quorum/assignment/role-transfer, persisted/volatile ACK, EOF/context/drain and copy-target/retarget traces pass. No old production stream/owner is removed; hosted parity waits for P5. |
| P4 Rust service and Kuberic host adapters | P2, P3 | Prepare protected facade, assignment/ACK APIs, helper-free and real KV/SQLite service ownership, native status/lifecycle adapter, named reports/schema/controller/election/rejoin cutover. | Adapters compile against resolved protected types; service-local commit/isolation and report/error cases pass. Old published evidence/wiring stays unchanged; no producer feeds new queue counters into old first/last. |
| P5 Atomic port cutover and old-engine removal | P2, P3, P4 | Switch sole Default factory, public services, tonic wire, host/schema/controller and lifecycle together; remove old stateless algorithm/RPCs and mandatory engine persistence/oracles. | Real public factory/host traces pass across Open/restart/build/catch-up/epoch/promotion/status/Close. Helper-free service needs no DurableState or extra commit signal; real KV/SQLite client durability/isolation stays safe. No dual engine/fallback remains. |
| P6 Custom-replicator and application closure | P4, P5 | PostgreSQL protected control semantics, partition/SQL admission, successor/catch-up policy, local idle removal, slots/WAL cleanup, stopped-progress snapshot and Abort grants. Reassess KV teardown and dynamic Open/settings guards. | All applicable protected control cases run against built-in and custom paths. Production PostgreSQL tests demonstrate denied SQL cannot commit, idle removal preserves others, slot cleanup, nonregressing relevant progress and disarmed retained gateway objects. |
| P7 Public capabilities and extension closure | P1-P6 | Complete capability ledger: settings/defaults/updates/retrieval, security enforcement, fault/health/load/move cost, metadata/data representation, optional extension obligations. | Every in-scope ledger entry has an implemented external case, including invalid/closed inputs and effective-settings round-trip. No silent/no-op capability, forced scope reduction or unreviewed protected-interface change. |
| P8 Documentation and compatibility qualification | P1-P7 | As-built reference, porting guide, corrected audit/KSF/API coverage, complete matrix across real service/provider and controller/archive journeys. | All rows pass under supported modes; exact inventory/archive/API privacy, ordinary and routine PostgreSQL gates pass. Remaining unsupported in-scope behavior blocks the complete-alignment claim. |

### Atomic Cutover Exit Contract

At the P1-P4 exits, the current public data path keeps **all** existing owners;
new component tests make no partial-parity claim. P5 has the following complete
replacement contract before any new default factory/API is published:

| Path | P5 owner/evidence; no old engine dependency remains |
|---|---|
| Open/restart | Protected provider recovery callback seeds queue progress; native empty/no-data normalization and error handling apply. No `DurableState::durable_progress` or provider-retained suffix is required to construct the core. |
| Copy ACK/completion | Receiver queue tracks context/copy/service ACK and terminal enumeration evidence. Snapshot prefix and later retained suffix use session queue state; no post-ACK store verification or durable-progress payload. |
| Retained replay/catch-up | Engine owns retained process-local bytes/cursors. Missing restart history triggers native copy/recovery, not an engine commit of application storage. |
| Primary local commit/recovery | Service owns transaction and visibility; a failed service commit faults/reconciles in the service. Engine quorum completion remains independent. It never invokes an application commit on ordinary writes, suffix settlement or epoch advance. |
| Reports/controller | Named native query values and query errors are atomically published/consumed. Provider-specific durable evidence is optional and explicitly absent when unavailable, never filled from queue counters. Election/rejoin/recovery cannot require an extra service durability hook. |
| Access | Native lifecycle/quorum calculator, not old applied==committed or optional provider-evidence presence. Service isolation is already implemented and exercised before enabling Granted states. |
| Promotion/epoch | Correct ACK-mode drain completes before queue transfer. Primary provider callback uses issued tail; secondary receive invalidation and stream/provider barriers have their own owner. No old storage reconciliation is called. |

Keep exact identity/history/session and fault/retirement correctness separate
from optional service evidence. P5 must run a real external **helper-free**
persisted StateProvider service through Open, target context/copy, replication,
promotion/recovery, restart and controller report handling. It implements no
DurableState, supplies no progress-bearing ACK and publishes no extra Kuberic
service-commit signal. Real KV/SQLite durability checks are additional, not
substitutes for that witness.

The cutover changes public types/wire/schema/controller and all supported
provider consumers together; old incompatible state rejects before mutation.
P2-P4 do not claim independently deployable native behavior. If P5 cannot
remove one old dependency while satisfying these gates, it does not ship.

P5 must revise old tests that intentionally encode a different contract:
retain their safety outcome, not an obsolete internal counter/status assertion.
For example, replace SF-01's per-apply denial expectation with service-isolated
visibility while native reads are Granted; preserve no uncommitted value/no
premature client success. SF-D02's historical committed scalar fix remains
historical evidence, not a reason to equate native queue progress with durable
local application commit.

### Existing Audit Traceability

| Findings | Planned owner |
|---|---|
| SF-20, SF-21 | P2/P3 native port, P4 adapters/P5 cutover; policy completion P7 |
| SF-18, SF-19 | P3 port/P4 adapters/P5 cutover; actual native receiver lifetime |
| SF-D01, SF-12 | P3 native queue/manager port/P5 cutover |
| SF-02 | P4 service adapter/P5 partition-status and write-owner cutover |
| SF-03, SF-07/08/09/13/16 | P6 |
| SF-15 | P5/P6 service-owned task completion |
| SF-17 | P7, with factory/service integration rechecked in P6 |
| Fixed SF-01/04/05/06/10/11/14 and SF-D02 | Preserve historical closure; requalify affected behavior in P2-P5/P8 |

Newly researched access, catch-up-target, queue-progress, epoch and outer-Close
differences are compatibility obligations above, not newly closed or silently
reclassified historical audit IDs. Inventory remains **7 fixed / 14 open**
implementation findings plus open SF-D01.

## Conformance and Rollout Gates

Use real public factory/host services, not only private field assertions.
Translate native queue, manager, sender, secondary, copy, role and public API
test inputs/actions/expected outcomes into Rust scenario vectors. Include the
native transition and Rust destination in each port-ledger row; do not satisfy
a native port gate only with Kuberic's previous tests. Maintain a
native-derived contract case ID per behavior, with barriers and
controllable clocks for assignment/receive/service ACK/local commit/EOF/
configuration transitions. Record source-derived expectations as such; do not
claim differential native execution without actually running it. Record three
evidence levels separately: source-derived native vectors, executed Rust/tonic
vectors, and actual native/port differential runs. The third is additional
evidence only when the native build/run is available; absence is reported, not
relabeled as a passing differential result.

Tonic integration scenarios must inject duplicate/lost/delayed ACKs, cross-stream
reordering, reconnect with stale endpoints, queue saturation and sender timeout.
Verify replication-before-copy ACK processing and session lifetime across role
transfer, not just bytes arriving on one ordered HTTP/2 stream.

Required service matrix: persisted sequential consumer; persisted parallel
consumer; volatile implicit; volatile required ACK; KV and SQLite real durable
providers; custom PostgreSQL applicable control paths. Include queue exhaustion,
out-of-order ACK, local commit failure after quorum, cancellation, late ACK,
process replacement, data-loss true/false, role transitions, pruned/empty history
and every status/terminal layer described above.

Keep existing ordinary/API privacy and source/archive exact-once gates. Add
scenario selectors/counts only when actual tests exist. Provider memory fixtures
do not prove fsync; real-provider client/commit tests stay required. Routine
PostgreSQL uses `just nextest-postgres-smoke`; new PostgreSQL tests need explicit
targeted execution, and full `just nextest-postgres` remains user-request-only.
An omitted supported-mode validation is reported as unverified, not passed.

No mixed-version or old-state migration guarantee is added. Public types,
wire/agent schema and supported provider formats can break coherently with
version/rejection tests; never reinterpret old evidence under new meanings.
The three protected interfaces remain unchanged. If a concrete incompatible
observation cannot be expressed through them and allowed companion surfaces,
stop for a user decision; no such impossibility was established by this research.

## Evidence Index

Kuberic links below are immutable at the current implementation baseline.
Native links use the verified pin above; some rows need multiple paths as noted.

- [N1]: [assignment/admission](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationQueueManager.cpp#L369-L411),
  [primary immediate ACK/quorum](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.cpp#L349-L454),
  [service-owned commit contract](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/managed/Api/src/System/Fabric/IStateReplicator.cs#L19-L81).
- [N2]: [persisted/required mode](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicator.cpp#L165-L193),
  [optimistic/explicit dispatch](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicationReceiver.cpp#L632-L740),
  [ACK callback states](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComFromBytesOperation.cpp#L167-L262).
- [N3]: [public stream contract](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/managed/Api/src/System/Fabric/IStateReplicator.cs#L102-L127),
  [target context/volatile omission](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicator.cpp#L1025-L1115),
  [receiver stream](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryCopyReceiver.cpp#L24-L80).
- [N4]: [copy/build capture](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.BuildIdleAsyncOperation.cpp#L111-L166),
  [pending pressure retarget](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.cpp#L745-L774),
  [periodic retarget](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.cpp#L1983-L2022).
- [N5]: [role queries](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.cpp#L296-L391),
  [received progress](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicationReceiver.cpp#L177-L202),
  [retained floors](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/OperationQueue.h#L62-L92).
- [N6]: [catch-up predicates](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.CatchupAsyncOperation.cpp#L42-L104),
  [current inputs](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.cpp#L1061-L1104),
  [captured boundary](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationQueueManager.cpp#L261-L362),
  [PC/CC constraints](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.cpp#L2328-L2528).
- [N7]: [access calculation](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp#L34-L145).
- [N8]: [direct Close](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.CloseAsyncOperation.cpp#L32-L200),
  [outer teardown](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.CloseAsyncOperation.cpp#L29-L109),
  [failed children](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.CloseAsyncOperation.cpp#L209-L276).
- [N9]: [primary epoch](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.UpdateEpochAsyncOperation.cpp#L31-L90),
  [secondary epoch](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicator.UpdateEpochAsyncOperation.cpp#L31-L156),
  [data-loss reset](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.OnDatalossAsyncOperation.cpp#L82-L157).
- [N10]: [public defaults](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/ServiceModel/reliability/replication/REConfigMacros.h#L140-L207),
  [settings update restrictions](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComReplicator.cpp#L762-L808),
  [partition reporting](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ComStatefulServicePartition.cpp#L164-L350).
- [N11]: [provider/operation buffer-array contract](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/idl/public/FabricRuntime.idl#L626-L683),
  [managed segment collection](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/managed/Api/src/System/Fabric/OperationData.cs#L16-L38).
- [N12]: [role state machine](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicatorState.cpp#L28-L248),
  [manager ownership](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.h#L285-L349),
  [queue contract](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/OperationQueue.h#L21-L92),
  [session ownership](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/RemoteSession.h#L202-L259).
- [N13]: [transport/demux](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationTransport.cpp#L880-L937),
  [reliable sender](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReliableOperationSender.cpp#L543-L817),
  [ACK processing order](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/RemoteSession.cpp#L492-L670).
- [N14]: [production interface exposure](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComReplicator.h#L12-L35),
  [unsupported internal batch/reserve methods](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComReplicator.cpp#L1002-L1034).
- [N15]: [native test target/scenario sources](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/test/CMakeLists.txt#L8-L30),
  [MIT license](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/LICENSE),
  [third-party notice inventory](https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/ThirdPartyNotices.txt).
- [K1]: [primary pipeline](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-runtime/src/replicator/stateless/mod.rs#L3288-L3455).
- [K2]: [persistence hooks](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-runtime/src/engine.rs#L19-L50).
- [K3]: [ACK and stream ownership](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-runtime/src/replicator/stream.rs#L173-L365).
- [K4]: [copy construction](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-runtime/src/replicator/stateless/mod.rs#L1948-L1990).
- [K5]: [progress/epoch/recovery](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-runtime/src/replicator/stateless/mod.rs#L2897-L3021).
- [K6]: [catch-up readiness](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-runtime/src/replicator/stateless/mod.rs#L3031-L3126).
- [K7]: [host access projection](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-runtime/src/host/hosting.rs#L545-L605).
- [K8]: [outer Close](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-runtime/src/host/public_lifecycle_program.rs#L438-L505).
- [K9]: [settings/capabilities](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-runtime/src/replicator/mod.rs#L283-L315).
- [K10]: [report production](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-runtime/src/host/hosting.rs#L665-L712),
  [descriptor consumption](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-controller/src/cluster_api/public_v1_operations.rs#L252-L276),
  [retained rejoin](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-controller/src/evaluator/public_v1_topology.rs#L706-L735).
- [K11]: [resolved protected stream types](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-runtime/src/application.rs#L31-L37),
  [protected provider signatures](https://github.com/youyuanwu/kuberic/blob/546269cb867dee1d53b4101987501e90df66e763/kuberic-runtime/src/traits.rs#L75-L100).
