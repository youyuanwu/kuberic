# Kuberic Test Strategy

Kuberic validation is split into exact nextest tiers, workspace checks,
provider-backed smoke tests, and explicitly owned Kubernetes scenarios.

The [faithful SF V1 Rust async/tonic port plan](sf-v1-semantic-alignment-plan.md)
adds native-source/test-vector mapping and a future behavior/capability matrix:
persisted/volatile services, service-owned primary commit, receiver lifetimes,
progress/quorum/status, terminal outcomes and real tonic retry/reconnect/
backpressure scenarios. Existing KSF inventory and current-contract regressions do
not establish complete native parity. P1's documented acceptance scenarios are
separate from future executable port-component/hosted cases; add selectors and
counts for those only with actual implementations. The unselected OperationQueue,
bounded sender/session/ACK, async/tonic, primary queue/quorum, secondary receiver/ACK
and copy-receiver slices have
actual component tests below. Preserve
safety outcomes while revising assertions that encode intentionally different
current contracts.

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
| ordinary | 775 |
| PostgreSQL | 41 |
| KinD live | 3 |
| build-only | 2 |
| **total** | **821** |

Ordinary partition counts are `194, 194, 194, 193`. PostgreSQL partition counts
are `11, 13, 8, 9`. The validator proves every test has exactly one tier and
every configured partition union is complete.

Source inventory measured after the copy-enumeration slice: 821 =
775 + 41 + 3 + 2. Discovery builds the tier binaries but does not execute
PostgreSQL or KinD bodies.

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

### SF V1 P1 Foundation

```bash
scripts/check_runtime_public_api.sh
```

P1 adds source-backed acceptance scenarios in the
[port ledger](sf-v1-port-ledger.md): 11 source families, 15 contract mappings,
33 deterministic scenarios and 29 capability/settings cases. They are
documented expectations for future tests of actual ported components, not new
ordinary tests or executable formula checks. The strengthened existing external
API fixture above checks real protected-interface compatibility. The existing
16 KSF entries remain structural/current-contract evidence; no new KSF ID or
native-parity claim is introduced.

The separate current-engine counterexample selection is:

```bash
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  --test-threads 1 --test public_v1_recovery \
  -E 'test(=read_fencing::sf_d02_primary_scalar_waits_for_quorum_and_local_commit_across_cancel_and_epoch) or test(=read_fencing::sf_d02_secondary_scalar_is_contiguous_service_acknowledged_not_committed) or test(=hosted_public_v1_contained_close_restarts_without_replaying_interrupted_children)'
```

These three existing real-engine tests preserve current held-ACK/held-local-
commit and contained-Close assertions. They do not execute expected native
algorithms. Five further current-source counterexamples in the ledger remain
source-only. Memory-backed current-engine witnesses prove ordering, not fsync.
The strengthened existing external API fixture pins Bytes, boxed `Stream<Item = Result<Bytes>>
+ Send`, i64 and all protected methods; it remains one of the **existing two**
build-only guards run by `scripts/check_runtime_public_api.sh`, not a new
ordinary or archive-only Cargo invocation.

### SF V1 OperationQueue Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::operation_queue::tests::)'
```

This selects **45** descriptive tests of the actual internal
[`OperationQueue`](../../../kuberic-runtime/src/replicator/v1/operation_queue.rs#L212)
and its
[`component tests`](../../../kuberic-runtime/src/replicator/v1/operation_queue/tests.rs#L73).
They cover separate tail/commit/completion/history boundaries, holes/duplicates,
count-span and byte pressure (including history removal on failed admission),
retention, callback timing/replay, rewinds, trim, reset and transfer. Q1–Q4/L2
remain provenance identifiers, not a duplicate executable catalog.
The original 42 queue tests are joined by three async/tonic-substrate adapter
regressions proving framed-operation logical-byte accounting without changing
the original raw-byte behavior.

The tests belong to ordinary and the existing all-feature archive automatically;
no new partition exemption or ignored tests are introduced. Source and archive
inventories must agree. This is source-guided Rust component evidence, **not**
native execution, differential parity, role/owner integration, full P2 or P5.
The queue remains unselected and the protected compiler fixtures are unchanged.
See the [ledger](sf-v1-port-ledger.md#operationqueue-slice-p2) and
[selected translation provenance](sf-v1-port-provenance.md#operationqueue-implementation-slice-p2-not-all-of-p2).

### SF V1 Sender Session ACK Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::sender::tests::) or test(replicator::v1::ack::tests::) or test(replicator::v1::session::tests::)' -j 2
# All preparatory v1 components, including the existing queue:
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::)' -j 2
```

The first selects **61 new actual component tests**: **28 sender**, **13 ACK
scheduler**, **20 bounded session**. Those families plus the 42 existing queue
tests account for **103**; the all-v1 selector also includes the later
async/tonic substrate below. They invoke real private compiled components with
injected/manual time, no guessed-deadline sleeps and no formula/catalog oracle.
Sender tests also invoke its production latency/decay helpers. They exercise
receive/apply separation, ordered replay/dedup, count windows, exact retries,
RequestAck, hints/backpressure, batching/force/final pending flush, trace,
coalescing/copy terminal state, reentrant dispatch and close/root lifetimes.
The initiating callback and processor ownership survive coalesced ACK arrivals.

Deadlines and timer-callback entry are component seams, not a live timer adapter.
Session copy/primary effects are explicitly delivered handoffs, **not execution
of missing copy/build/quorum owners**. Tests demonstrate component order,
ownership and progress; they do not prove demux, carrier/error mapping, provider
durability or native/hosted parity. Native tests/sources were read, not executed.
The production factory, public aliases and protected interfaces are unchanged.
These tests are ordinary, automatically discoverable by the existing archive/
partition rules; no ignored test, catalog ID, tier exemption or dependency added.
See [scope/remaining owners](sf-v1-port-ledger.md#sender-session-ack-slice-p2-bounded-f06)
and [source/test provenance](sf-v1-port-provenance.md#sender-session-ack-implementation-slice-p2-bounded-f06).

### SF V1 Async Tonic Substrate Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::buffers::) or test(replicator::v1::transport::) or test(replicator::v1::async_runtime::)' -j 2
```

This selects **51 actual private component tests**: 27 async runtime, 20
carrier and four buffer adapter. Together with three additional queue-accounting
regressions selected above, the slice adds **54**, taking all-v1 coverage from
103 to **157**. A cross-component test connects the actual session runtime,
send-intent adapter, tonic exchange, reverse ACK runtime and ordered session
ACK processor; required copy-owner handoffs are recorded, not fabricated.
Explicit raw-single/framed-segments cases preserve arbitrary bytes,
zero/empty/multiple buffers, per-item stream errors
and ordinary EOF. Malformed frames fail rather than becoming empty success.
Carrier cases keep routing/incarnation/stream ownership, independent receive/
apply ACKs, per-message expiry, count/byte pressure and failure outcomes
separate from service/quorum completion. Paused Tokio time drives actual
session retry and ACK batching/force/close callbacks, not test-only formulas
or guessed wall-clock sleeps.

The private proto/service is built but not registered with the selected host.
Public types, factory selection and current RPCs remain unchanged. The
external negative compiler fixture explicitly rejects access to `replicator::v1`.
No native test/binary, production activation, copy/build/quorum owner, full P2
or hosted/native/differential parity is claimed. See the
[bounded ledger](sf-v1-port-ledger.md#async-tonic-substrate-slice-p2-bounded-f05f08f11)
and [provenance](sf-v1-port-provenance.md#async-tonic-substrate-slice-p2-bounded-f05f08f11).

### SF V1 Primary Queue and Quorum Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::replication_queue_manager::) or test(replicator::v1::replica_manager::)' -j 2
```

These tests invoke actual private managers, queue entries, sender sessions and
ordered ACK processors. Successful assignment is observable before quorum
completion; full/malformed admission does not consume an LSN. Held ACKs,
configuration changes and outside-owner dispatch exercise current/previous
quorum, quorum-one asymmetry, receive-based reclamation versus apply-based
catch-up, idle retention, configuration capture/caps and moving MustCatchup
requirements. Dropped waiters do not roll back admitted operations; terminal
completion and close outcomes remain distinct.

The slice adds **44 actual tests**: **13 primary queue owner** and **31 replica
manager**, taking all-v1 coverage from 157 to **201**. The integrated runtime
library selection passed **258/258**, including the 57 other library tests.
Workspace checks, warnings-denied Clippy, no-default-feature runtime, doctests,
external API/privacy and exact tier/partition inventory passed. This is not a
claim that all 570 ordinary tests were rerun.

Review regressions preserve cached CC quorum/all observations until the ordered
primary-progress handoff, concrete session-root identity, error-atomic
configuration/idle removal, and shutdown errors after terminal cleanup.
Out-of-range session ACKs cannot become valid by admitting more operations
while configured quorum membership is insufficient.

Queue-only transfer tests preserve retained original epochs and the issued-tail
assignment gap. They do not execute full promotion, demotion trimming, provider
epoch callbacks or receiver drain. Concrete established-session inputs are not
the full native descriptor-driven configuration/build lifecycle. No selected
factory, protected interface, service persistence hook, dependency, ignored
test, archive exemption or duplicate executable scenario catalog is added.
Slow-fault policy/notification, full role/copy/secondary owners, native or
differential execution, hosted parity and activation remain unestablished.
See the [bounded ledger and owner map](sf-v1-port-ledger.md#primary-queue-and-quorum-slice-p3-bounded-f02f06f09)
and [selected provenance](sf-v1-port-provenance.md#primary-queue-and-quorum-slice-p3-bounded-f02f06f09).

### SF V1 Secondary Receiver and ACK Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::received_operation::) or test(replicator::v1::operation_stream::) or test(replicator::v1::secondary_receiver::)' -j 2
```

The slice adds **92 actual tests**: 31 received-operation, 22 operation-stream
and 39 receiver/integration, taking all-v1 coverage to **293**. The integrated
runtime library selection passed **350/350**, including 57 other library tests.
Workspace all-target/all-feature check, warnings-denied Clippy, no-default
runtime, doctests, external API/privacy and exact tier/partition inventory passed.
This is not a claim that all 662 ordinary tests were rerun.

Actual queues and callbacks exercise required versus optimistic completion,
out-of-order data/service ACKs, duplicate/root identity, logical byte/span
pressure, completed hints and capability. Actual shared streams exercise
wait/wake/cancellation, reacquisition, EOF/EOS and explicit epoch handoffs.
Provider invocation is not a successful stub: full barriers pause until the
captured ACK boundary and explicit matching completion; stream markers block
all getters. Tests preserve original versus sending/copy epochs and callback
replay through successive barriers. Abandoned work does not auto-complete.

Real AckSender tests use copy-plus-replication pending thresholds and current
receiver snapshots. A real primary queue/manager/send/receiver/ACK/session
trace proves an ACK cannot bypass the ordered primary notification; its
unimplemented copy-owner handoff stays explicitly unresolved. It does not
execute build/role/provider integration or a selected hosted engine.

Receiver failure containment and sticky stream abort are explicit Rust adapter
boundaries; native fault reports, two-receiver EOS/drains, provider callbacks,
copy/context/build and full secondary role transitions remain future owners.
No protected interface, selected factory/wire, dependency, archive exemption,
ignored test or duplicate executable scenario catalog is added. Native binary/
test/differential, provider-fsync and hosted parity are not established.
See the [bounded ledger](sf-v1-port-ledger.md#secondary-receiver-and-ack-slice-p3-bounded-f04f05f06f09)
and [source provenance](sf-v1-port-provenance.md#secondary-receiver-and-ack-slice-p3-bounded-f04f05f06f09).

### SF V1 Copy Receivers Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::secondary_copy_receiver::) or test(replicator::v1::copy_context_receiver::) or test(replicator::v1::copy_receiver_integration::) or test(numbered_copy_eos_)' -j 2
```

The slice adds **68 actual tests**: 31 secondary-copy, 33 copy-context, two
numbered-EOS constructor and two pair/session integration cases. Combined V1
coverage is **361**; the integrated runtime library passed **418/418**, including
57 other library tests. Workspace all-target/all-feature check, warnings-denied
Clippy, no-default runtime, doctests, external API/privacy and the exact
776-test tier/partition inventory passed. Full ordinary, PostgreSQL and KinD
test bodies were not rerun.

Actual secondary-copy/context receivers reuse OperationQueue, ReceivedOperation,
SharedDispatchQueue and AckSender. Cases exercise count-bound gaps/duplicates/
retries, required versus optimistic completion, canonical numbered network EOS,
legacy last markers, logical-empty framed payloads, shared consumption and
terminal reacquisition. Callback selection/entry/completion, late exact identity,
root/destructor/waker reentry, local cleanup differences and rejected inputs
are exercised on real owners, not a copied ledger/formula model.

Context cases exercise target-to-source data receipt without service ACK,
strict pending threshold, current issuance snapshots, final error ACK and
explicit-close versus protocol-EOF outcomes. Pair/session witnesses exercise
the actual combined copy+replication pending threshold and terminal ACK
sentinels, stopping before the missing copy-build owner.

Provider invocation, context acquisition/copy enumeration/sending/build,
StartCopy establishment, cross-receiver fault/report/EOS/drains and full role
integration remain future owners. No protected type, selected production path,
dependency, ignore/exemption or native/differential/hosted parity claim changes.
See the [bounded ledger](sf-v1-port-ledger.md#copy-receivers-slice-p3-bounded-f04f05f07)
and [source provenance](sf-v1-port-provenance.md#copy-receivers-slice-p3-bounded-f04f05f07).

### SF V1 Copy Enumeration Slice

```bash
cargo nextest run -p kuberic-runtime --lib --all-features --profile ordinary \
  -E 'test(replicator::v1::copy_async_operation::)' -j 2
```

The slice adds **45 actual tests**, for **406 combined V1** and **463/463**
passing runtime library tests (57 unchanged non-V1 cases). The exact selector
passes 45/45. Workspace all-target/all-feature check, warnings-denied Clippy,
runtime no-default-feature check, external API/privacy, workspace doctests and
exact 821-test tier/partition discovery passed. Full ordinary, PostgreSQL and
KinD test bodies, native execution/differential, hosted parity and provider-fsync
were not run or claimed.

Actual CopyAsyncOperation tests use the real OperationQueue, reliable sender,
buffer adapter and injected clock. They exercise serial ready/pending provider
polls, empty data versus numbered EOF, count growth, one parked item, received
versus applied ACKs, duplicate/regressive ACKs, parked EOF notification, retry
and RequestAck, native cancellation timing and late provider-error precedence.
Native twelve-item drained-success and fifteen-item/eight-slot/full-retry
cancellation cases are adapted without the native harness. Real-executor
cases prove ACK wake/resume and in-flight cancellation; dropping a drive future
preserves its pending provider poll, not a successful native cancellation.

Rooted selected work retains the provider through End/owner release. Provider
polls, EOF/send callbacks, driver wakers and rejected/retired payload destructors
are exercised for outside-guard reentry. Framed-empty data, malformed settings/
ACK/frame and sequence exhaustion have actual regressions. Actual secondary
copy receivers exercise both service-ACK modes; actual primary-side context
receipt hides the protocol terminal and ACKs its LSN.

Final-copy readiness stays separate from an explicit outer-owner success request.
Receiver-boundary tests do not implement CopySender's sentinel conversion,
replication build gate, EOF-issued target or retargeting. Provider invocation/
context acquisition, StartCopy establishment, full fault/drain/role integration
and slow-provider API monitoring/health reporting remain pending, as does
production activation. No protected API, selected factory/
wire, dependency or tier/exemption policy changes.
See the [bounded ledger](sf-v1-port-ledger.md#copy-enumeration-slice-p3-bounded-f05f07f11)
and [source provenance](sf-v1-port-provenance.md#copy-enumeration-slice-p3-bounded-f05f07f11).

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

### Built-In Read Visibility and Prefix Repair

```bash
cargo nextest run -p kuberic-runtime -p kvstore2 -p kuberic-level-tests \
  --all-features --profile ordinary --test-threads 1 \
  -E 'test(sf01_) or test(sf11_committed_prefix_)'
```

Twelve focused tests cover pre-provider publication failure/drop/revocation,
real normal KV reads behind primary/secondary post-visible apply barriers,
unavailable required quorum, genuine settlement, mutate-then-error, caller
cancellation, terminal authority and equal raw counters without settlement.
The hosted quorum-three fixture injects third-member socket-route failure;
the in-process primary fixture physically cuts both peers' TCP routes.
The exact SF-11 matrix records primary `(11,10)`, peer `(5,5)` replay of
6-11 before suffix commitment and genuine divergent/missing-history rejection.
See the [audit's evidence and scope](service-fabric-contract-audit.md#sf-01-and-sf-11-revalidation);
these tests establish new normal-read admission, not isolation of already
admitted reads or arbitrary application data views.

### Role-Specific Scalar Progress

```bash
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  --test-threads 1 \
  -E 'test(sf_d02_) or test(sf11_committed_prefix_) or test(primary_write_) or test(=commit_failure_permanently_contains_the_ambiguous_suffix) or test(=hosted_public_v1_authorized_data_loss_maps_only_the_effective_outcome)'
```

Two new hosted tests query the actual public control and a journalled Progress
outcome while a real peer service ACK is held after local apply, then while local
commit is held after quorum. They cover success, joined caller cancellation,
a completed epoch barrier, secondary pre/post-service-ACK acceptance and gap
rejection. No elapsed-time assertion supplies the safety proof. The existing
SF-11 matrix also queries opened None, secondary-to-primary promotion and all
successful/failed suffix-settlement outcomes. Extended commit-error, close/epoch
and nonempty typed Preserve recovery cases distinguish scalar commitment from
independent report tails and access. These memory-backed tests prove protocol
sequencing, not fsync; existing real-provider SF-01 regressions remain separate.
See [SF-D02 closure](service-fabric-contract-audit.md#sf-d02-primary-progress-promise-exceeds-the-built-in-query-result)
and the [role/native-source qualifications](service-fabric-api-semantics.md#role-specific-progress-and-recovery).

### Quiet Snapshot Boundary Proof

```bash
cargo nextest run -p kuberic-runtime --all-features --profile ordinary \
  --test-threads 1 -E 'test(sf10_)'
```

Five public factory/host tests complete quiet empty/nonzero snapshot-only
Build, activate, and run both catch-up modes without another write or rebuild.
They cover explicit progress and already-known-peer `INVALID_LSN`, including
no retained boundary record on either side. Other cases cover concurrent and
later exact suffixes, conflicting later payload, exact read-only proof retries,
mixed proof/data rejection, wrong receipt/session/configuration/history/storage,
missing live durable progress, and source abort during delayed proof work.
The source-abort barrier delays the outbound proof, not a service operation:
the prefix proof intentionally performs no application mutation.

Disabling only the receipt-proof branch makes both quiet nonzero journeys
fail with `IncompatibleRetainedHistory`; empty cases pass. Memory-backed
fixtures prove protocol sequencing, not fsync. Existing real-provider,
service-ACK/cancellation and SF-01/SF-11/SF-D02 regressions remain independent.
See [SF-10 closure](service-fabric-contract-audit.md#sf-10-quiet-snapshot-builds-fail-boundary-proof)
and [proof semantics](service-fabric-api-semantics.md#completed-snapshot-prefix-proof).

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

Twenty-five ordinary-tier journey/assertion tests cover bootstrap/status conflict,
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
