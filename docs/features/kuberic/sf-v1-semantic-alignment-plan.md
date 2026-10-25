# SF V1: Hosting Redesign Roadmap and Remaining Qualification

This is the **single active runtime roadmap**, not a delivery-phase checklist.
**M1 generic host/controller implementation is delivered**; **M2 and M3 remain
PROPOSED / NOT IMPLEMENTED**. The [M1 contract and evidence limits](ordinary-hosting-m1.md)
supersede old production-host claims. The design research baseline is Kuberic
`b37009f8af7a7b2ed91f58f4889ed74e06013861`;
native SF is `3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`.
Older bounded access/host evidence retains its explicit pins below.
Read [current semantics](classic-v1-runtime.md) first and use the
[disposition ledger](sf-v1-port-ledger.md) to interpret historical SF IDs.
Completing P6 and replacing an old engine do not prove that no core defect remains.

Priorities below order **future separately authorized work**. “Verified difference” means
source-confirmed divergence, not a dynamically reproduced data-loss defect;
“unverified” means missing claim-specific qualification, not demonstrated failure.

<a id="hosting-redesign-roadmap--not-implemented"></a>

## Hosting redesign roadmap

The [target owner/message/state contract](engine-neutral-hosting.md#proposed--ordinary-engine-boundary)
removes the mandatory custom companion and controller-to-target exact-grant
boundary, rather than renaming it. Native source confirms the ordinary interface/
host/engine separation; generic peer identity publication, durable agent stages,
engine-session retirement and Rust Close-after-Abort settlement are explicit
local **design choices**, not native public APIs.

**M1 is implemented under separate runtime authorization. M2/M3 still require
separate authorization.** M1 qualifies a real production ordinary-engine path,
not the built-in/PG replacement sessions or exhaustive recovery matrix. No native
executable equivalence is claimed. M1–M3 replace the earlier R1–R5 cutover plan
with a clean-break implementation for fresh deployments. Independent
SF-11/mode/PG/P7/security obligations below remain open/deferred, not erased.

| Milestone / dependency | Future deliverable | Completion gate |
|---|---|---|
| **M1 — IMPLEMENTED: host/controller contract**. No predecessor. | Format-15 controller goals, durable local-agent stages, retained ordinary host actions/remote state and **production** goal→PrepareIdle→Build routing. DiscoveryPending→AchievedScope publication lives in existing components, with no parallel authority ledger. | [Executed independent ordinary-engine acceptance](ordinary-hosting-m1.md#verification-and-limits): no companion/diagnostics; mutual discovery with multi-replica recovery/progress held and copy/voting/serving closed; failed-preparation withdrawal, stale/reply-loss/two-ended Ready and Abort→Close actual joins. Protected method sets unchanged. Built-in/PG production integration is explicitly unavailable pending M2. |
| **M2 — NOT IMPLEMENTED: built-in and PostgreSQL integration**. M1. | Replace grant-based copy with engine-owned authenticated sessions and ordinary lifecycle joining. Built-in retains stream/context/quorum semantics; independent PG retains backup/WAL/lineage/slots, physical SQL fencing and its durable metadata responsibilities. Delete superseded companion/grant/carrier paths as their consumers move, not in a later compatibility phase. | Both engines through M1 production routing, with real PG and no built-in wrapper. Built-in: nonempty target context/empty items/EOF, copy+replication ACK boundaries, duplicate StartCopy and stale process/storage/history/endpoint/epoch/ACK rejection. Both: delayed establishment after retirement, lost target cancellation while source stays Primary, pre-effect/await races, quarantine, no late successor install and actual joins. PG additionally proves receive+replay/checkpoint, live replay without mutation, slot retirement, SQL/pre-auth socket fencing and sticky join failure. |
| **M3 — NOT IMPLEMENTED: recovery and failure qualification**. M2. | Complete and qualify goal/stage reconciliation for renewal, replacement, failover and handoff using only the replacement path. Verify same-format crash recovery, clean deployment and complete legacy-path removal. | Source/target/controller restart at selection/effect/receipt gaps; target reopen invalidates old Ready; remove-before-Build tombstone and failed-Build ambiguity; held multi-replica discovery with copy/access closed until AchievedScope. PC/CC/catch-up/promotion/demotion, actual KV/SQLite/PG access, acknowledged-prefix/history preservation, retained joins, protected API/package proof and incompatible-format rejection **before mutation**. |

### Fresh-deployment contract and removal order

Current host/control/CRD inputs use matched format 16; the
[M1 format-15 checkpoint](ordinary-hosting-m1.md#fresh-deployment-format) is historical.
protocol 13 / schema 14 are the historical baseline, not supported deployment input.
The replacement supports **fresh deployments only**, with matching binaries
and newly provisioned compatible metadata/application storage. Allocate the
replacement format versions during M1; do not reinterpret current payloads.
Reject incompatible stores, commands and retained CRD intents before mutation.
There is **no** old-state import, in-place upgrade, rolling/mixed-version mode,
downgrade reader or reverse migration. Do not build converters or software
rollback machinery.

Never automatically delete, reset or reinitialize existing application data or
PGDATA to make it fit. Encountering earlier-format deployment state is an
explicit unsupported-deployment error, not permission to clear storage.
Provision fresh resources rather than reuse a live deployment's storage.
This is a version-transition policy, **not a restart strategy**: matching-format
deployments must still recover their existing metadata and application data,
including interrupted transactions, uncertain operations and acknowledged state.

Removal ordering is property-based: inventory each old grant/journal/carrier/
coordinator assertion → qualify replacement at host or actual engine owner →
delete its old path/fields/fixtures as consumers move. Do not
drop stale/late/cancel/join negatives simply because their old API disappears.
No deployed legacy fallback is supported. Milestones are development checkpoints,
not separately deployable modes; intermediate commits need not support the old
stack. M3 verifies that no superseded production path remains, rather than
introducing a final cutover subsystem.
Host goal/outcome/ambiguity durability remains; target ingress journals do not
become engine requirements. Same-live-host Ready reuse requires both endpoints'
process/creation/storage and scope unchanged, with target publication rechecked
before voting. Source Remove never certifies remote receiver drainage.

### Validation entry points and qualification limits

[M1 production/host selectors](testing.md#ordinary-hosting-m1) now exercise the
replacement contract. [Historical baseline selectors](testing.md#hosting-redesign-baselines-and-future-acceptance)
map earlier host, built-in, controller, public workspace and real PG witnesses.
Old positive controller journey names now assert explicit M2 unavailability;
their later clauses are not executed M2 evidence. List the exact live selection before future authorized
execution; add missing race/crash/fresh-deployment cases at replacement owners rather
than inventing evidence or hard-coding test totals.

PG metadata removal stays separately deferred behind its
[field-by-field reconstruction gate](../postgres/stateless-metadata-prerequisites.md).
The old generic fresh-floor/KSF prescription is superseded, not imported into
this roadmap; existing native catch-up predicates and PG All/capability fallback
remain. No marked-successor PG WriteQuorum, orphan self-fencing, MySQL
qualification, production security or native differential evidence is supplied.
Downstream engines must independently qualify the
[ordinary target integration obligations](engine-neutral-hosting.md#migration-and-unresolved-proof-risks).

## Delivered — Native-style engine-neutral hosting

Merged #198 supplied the regression baseline; the separately scoped bounded
port is implemented through `91a2a93c3c7cf9477adb3a0d27e674b99f741a8a`.
The [message/action/state map](engine-neutral-hosting.md) records pinned native
evidence, common host admission/authority/lifetime owners, optional diagnostics,
exact remote Build state and intentional Rust/Kubernetes adaptations.

Pinned pre-M1 V1 and independent PostgreSQL controller dispatch qualified real copy
outside voting, truthful host/engine observations, separate admission, exact
retry/revoke/cancel and successful/failed hosted terminal joins. Production
failed-callback repeatability remains conservative; native observation ownership
does not certify an application's failure or a native join. Historical success
never created a new grant or an engine idle observation. Current hosted wrappers
check explicit M2 unavailability; they do not rerun these historical positives.

This closes the bounded structural hosting queue item, not wholesale Hosting2,
RA/FM, P7/security/settings, arbitrary failed-Build recovery, native executable
equivalence, downstream MySQL qualification or production certification.
It does not close the newly distinguished **ordinary extension-boundary**
divergence. Merged #199/#200/#201 are completed baselines, not missing roadmap
phases. M1–M3 replace the affected project direction, not their acceptance record.

## Selected-host Close and access disposition

### Outer Close outcome

The current common journal deliberately retains a failed public result after a
child Close error, even when actual cleanup succeeds. Positive containment and
successful Close are different facts; failed/lost drainage never becomes a
positive witness. This supersedes the earlier native-style success-shaped outer
Close disposition, while preserving cancellation-independent terminal joins.
See the [current terminal contract and actual engine witnesses](engine-neutral-hosting.md).
Unrecorded destructive callbacks are contained without reinvocation; recorded
Cleanup is not a process-local join or a generic application-task join.

### Access during reconfiguration

The selected runtime now publishes same-primary, split/recipe promotion and
standalone/composed demotion stages from operation-owned live evidence
([policy:209–300][aligned-access], [public barriers:144–2159][aligned-access-tests]).
The [current matrix and protocol-13 boundary](classic-v1-runtime.md#host-access-lifecycle-and-retirement)
replace the former blanket pending/fence availability difference. Public tests
attempt committed service reads and raw writes, including independent missing
C/P, pre/post-Revoke, final-wait-before-receipt, cancellation, exact replay,
late owner and final release. Descriptor availability is not live health
certification; service isolation and the actual shared admission gate remain
independent. Previous/current primary context and same-live-host repromotion
after interrupted/superseded demotion have selected witnesses; nonactivating
operations do not erase outstanding reconciliation. This is bounded source-guided alignment, not exhaustive native
stage/interleaving qualification.

## Scoped rejection-order disposition

The [scoped contract](classic-v1-runtime.md#rejection-and-fault-scope) has
current actual-owner and public-host both-order witnesses
([owner:18–230][rejection-owner], [public:57–392][rejection-public]).
Dispensable remote idle-copy failure and an actual selected target's
configuration-denied Exchange do not defeat sufficient healthy progress.
Required PC/CC/marked/All evidence remains required; new entries after
primary-local Build faults reject separately. Targeted vectors passed with
the [documented selectors](testing.md#scoped-rejection-order-and-local-fault-evidence).

This addresses the bounded current qualification obligation, not the exact
removed-engine typed-error API, every remote fault class, native executable
equivalence or complete alignment. Historical SF-12 remains a finding against
the removed implementation, not a discovered native/current rejection defect.
Broader qualification below still requires separate authorization.

## Source-contract disposition

The [protected catch-up Rustdoc][catchup-doc] describes selected classic V1's
moving All and captured-boundary/current-quorum/live-marked WriteQuorum
conditions ([manager:975–1024][catchup]). The
[provider epoch Rustdoc][epoch-doc] distinguishes primary issued-tail callbacks
([manager:292–302][epoch]) from secondary stream boundaries. These contracts
do not certify provider commitment, durability or client success.

## P2 — Focused qualification, not presumed defects

| Obligation | Current evidence / limit | Bounded next work and completion criterion |
|---|---|---|
| **SF-11 historical trace shape** | Current prefix regression preserves acknowledged prefix before ambiguous suffix ([test:146–214][prefix]); it is not the removed-engine `(11,10)` reconciliation against peer5. | Define a narrowly justified selected-path retained-source/behind-peer case before adding a test. Record what it proves about acknowledged-prefix recovery and what remains outside it; do not claim historical negative controls ran now. |
| **Public copy context — bounded obligation satisfied** | Separate from [component:1013–1091][context], the executed helper-free [public Build][public-context] consumes exact target-origin `[nonempty, empty, nonempty]` through EOF with zero source-context acquisition, meaningful fresh copied state, matched successful Build/Ready and successful Close/wait/drain. Standalone runtime features are `host` without `testing`; the workspace wrapper is testing-unified. | [Reproduction and limits](testing.md#public-target-origin-copy-context). The historical [empty-only provider][empty-context] remains the default journey, not the nonempty witness. No runtime production/interface change was needed. Idle copied state is not voter/serving admission, arbitrary failed-Build recovery, native execution or production certification. Other P2/P7 obligations remain separate. |
| **Mode/fault/interleaving coverage** | Persisted/required-ACK combinations are core-tested ([tests:121–172][modes]); selected production has fixed mode/policy choices ([production:121–158][policy]). | Build a claim-specific component/public matrix and close consequential holes only. Private required-ACK volatile mode is not an exposed public setting; do not expand P7 simply to fill coverage cells. |
| **PG terminal observations** | Orderly stopped progress and retained Abort cleanup have distinct witnesses ([tests:163–249][pg-progress], [940–1050][pg-abort]). | Separately scope any need for SQL racing shutdown, Abort progress snapshots or survivor ranking. Completion requires an actual bounded scenario; existing fsynced files and process cleanup do not establish these claims. |

## Custom-native controller evidence and remaining obligations

The **pre-M1** [local companion](replicator-boundary.md#custom-controller-integration)
and [pinned PostgreSQL controller cases](https://github.com/youyuanwu/kuberic/blob/ce9df4ad85603b935752ed7184d9314a778b6eb5/examples/postgres/src/public_controller_tests.rs)
provided bounded upstream evidence for exact inbound authority, truthful installed
voting/idle observations, real copy before admission, exact joint/current-only
staging, native replay/invalidation, SQL/replay and completed fencing. Fresh
sentinel initialization requires exact successful New-Open provenance; it is
not zero progress or a retained-history election. Protected native method sets
and representations remain unchanged. Current hosted wrappers check explicit
M2 unavailability; their later copy/SQL/replay/fencing clauses do not execute.

Remaining claim-specific obligations are not presumed defects or implied runs:

- **Pre-M1 retained engine/component route:** downstream MySQL needed the companion and
  must requalify its own
  source/target Build, native lineage/progress, later admission and actual access
  fencing with exact process/storage/history/attempt bindings. Its unpublished
  fixtures and externally reported seven-completion trace were not reproduced.
  **Current generic M1 route:** no companion is required. Actual downstream
  engine sessions/integration remain to be qualified independently alongside
  M2's built-in/PG work; native transfer/lineage/access/terminal obligations
  remain, with no downstream evidence claim.
- Native SF executable/differential validation, arbitrary failed custom-Build
  reformation and production deployment remain outside the bounded upstream
  witness. Failed/interrupted custom work may require explicit terminal cleanup
  or process replacement before conflicting work can proceed; a historical
  completion is not a renewed ingress grant.
- Marked-successor PostgreSQL WriteQuorum remains explicitly unsupported as
  documented below. Generic authority support does not implement missing native
  database capabilities, broad P7 settings or new transport-fault controls.

## Deployment security qualification

This is not all a rare-extension concern. Production requires HTTP and checks
agent bearer credentials, without TLS
([transport:177–227][http], [728–766][auth]).
Current deployments need an appropriately isolated/protected network, token
handling and namespace/resource controls. No production-security certification
is established by the source checks.

**Bounded next work:** document the intended deployment threat model and
separately qualify network/token/resource controls, or design transport
protection under new authorization. **Completion criterion:** actual deployed
configuration and negative access/credential/network cases support each claimed
protection. Do not imply missing public SF credential-update APIs are accepted
or enforced; they are a different capability obligation below.

## Deferred broad P7 — Still in the original target

The user deferred broad capability expansion **for now**, not because it was
implemented or outside the original alignment target. Existing declarations,
bearer enforcement, basic reporting and private algorithms must not be relabeled
wholly absent.

| Deferred family | Present boundary and remaining original-target obligation |
|---|---|
| Effective settings/defaults/update/retrieval | Address-only public settings and unchanged effective-endpoint update exist ([settings:275–283,390–438][settings]). Full native merge/validation/upgradeability/getter semantics remain ([native Com:490–808][native-settings]). |
| Queue/message/history/Close policies | Fixed production queue/sender/ACK/payload/prewait values exist ([production:40–66][fixed]); native public/internal defaults and settings precedence differ ([native macros:136–207][defaults]). Public configurability is not complete. |
| Persistence, ACK/EOS and operation data | Lifecycle persistence declaration drives selected receiver mode ([production:121–158][policy]); core supports more combinations and a private segmented representation. Public policies/stream-fault/EOS/metadata and lossless buffer exposure still need explicit capability decisions and qualification. |
| Listener/address and credentials | HTTP/bearer carrier exists; native credential-settings/retrieval/update and corresponding peer enforcement are not exposed. TLS absence is an actual deployment constraint, not proof of unauthenticated transport ([native Com:762–808][native-credentials], [transport:728–766][auth]). |
| Scoped fault/health/load/move-cost reporting | Basic partition load/fault methods exist ([partition:1036–1064][partition]); full native scoped reports/batching/clear/convergence behavior is not qualified ([native partition:208–350][native-partition]). |
| Actual interface boundaries | Native exposed interfaces, E_NOTIMPL/internal-only methods, ServiceGroup atomic wrappers and V2 copy-latest are distinct. Do not infer capability from every SDK declaration ([native QI:12–35][qi], [Com:1002–1118][internal]). See [ancestry register](sf-v1-port-provenance.md#native-exposure-and-settings-targets). |

**Re-entry/completion criterion:** when authorized, select a bounded capability,
retain exact source/default/validation semantics and explicit unsupported
outcomes, then demonstrate effective settings, peer enforcement and
closed/invalid-input/failure behavior through the selected public path. Keep
PostgreSQL marked-successor WriteQuorum explicitly unsupported unless separately
implemented; ordinary unmarked WriteQuorum/All and capability-false All fallback
already have current evidence ([PG:1476–1504][pg-wait],
[controller:445–470][fallback]).

## Broader qualification and claim discipline

Native binary/differential execution, exhaustive mode/fault/interleaving coverage,
kernel power-loss durability and production readiness are **unverified**, not
achievements of this rewrite or implications of P6. Real KV/SQLite files,
PostgreSQL processes, component tests and public/controller journeys each have
different scope. [Testing](testing.md) describes reproduction tiers;
[provenance](sf-v1-port-provenance.md) records translated versus source-derived
expectations and exclusions.

Future broad qualification needs an explicit mode/capability/claim matrix,
actual commands/environment/outcomes and bounded success criteria. It must not
become an unprioritized resumption of P1–P8 checkboxes or treat these two bounded
host changes as complete alignment. The selected regressions and documentation
do not satisfy the entire original P8 qualification claim.

[aligned-close]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/src/host/public_lifecycle_program.rs#L550-L638
[aligned-close-tests]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/tests/public_v1_recovery.rs#L3114-L3505
[aligned-access]: https://github.com/youyuanwu/kuberic/blob/69b1cfe3c85d296463da0def3e80eaca0ddcd0a7/kuberic-runtime/src/host/selected_access.rs#L209-L300
[aligned-access-tests]: https://github.com/youyuanwu/kuberic/blob/69b1cfe3c85d296463da0def3e80eaca0ddcd0a7/kuberic-runtime/tests/support/staged_access.rs#L144-L2159

[minority]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L2302-L2433
[marked]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replica_manager/tests.rs#L690-L719
[prefix]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/support/read_fencing.rs#L146-L214
[context]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/copy_establishment/tests.rs#L1013-L1091
[empty-context]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/scripts/fixtures/classic-v1-provider/src/lib.rs#L196-L207
[modes]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/secondary_receiver/tests.rs#L121-L172
[policy]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L121-L158
[pg-progress]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1_p6_tests.rs#L163-L249
[pg-abort]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1_tests.rs#L940-L1050
[catchup-doc]: ../../../kuberic-runtime/src/replicator/traits.rs#L107
[epoch-doc]: ../../../kuberic-runtime/src/traits.rs#L53
[catchup]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replica_manager.rs#L975-L1024
[epoch]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replica_manager.rs#L292-L302
[http]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L177-L227
[auth]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L728-L766
[settings]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/mod.rs#L275-L438
[native-settings]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComReplicator.cpp#L490-L808
[fixed]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L40-L66
[defaults]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/ServiceModel/reliability/replication/REConfigMacros.h#L136-L207
[native-credentials]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComReplicator.cpp#L762-L808
[partition]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/mod.rs#L1036-L1064
[native-partition]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ComStatefulServicePartition.cpp#L208-L350
[qi]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComReplicator.h#L12-L35
[internal]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComReplicator.cpp#L1002-L1118
[pg-wait]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L1476-L1504
[fallback]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-controller/src/evaluator/public_v1_topology.rs#L445-L470
[rejection-owner]: ../../../kuberic-runtime/src/replicator/v1/primary/tests/rejection_order.rs#L18-L230
[rejection-public]: ../../../kuberic-runtime/tests/public_v1_recovery/rejection_order.rs#L57-L392
[native-host-open]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ComProxyStatefulService.OpenAsyncOperation.cpp#L17-L39
[native-host-proxy]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ComProxyReplicator.h#L17-L35
[native-host-build]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReplicatorBuildIdleReplicaAsyncOperation.cpp#L256-L357
[public-context]: https://github.com/youyuanwu/kuberic/blob/45749d3744b43095be9bc9ec3fb4abf20633a537/scripts/fixtures/classic-v1-provider/tests/public_copy_context.rs#L377-L647
