# SF V1: Current Source and Evidence Ledger

The separately scoped [common hosting port](engine-neutral-hosting.md) is
implemented through `91a2a93c3c7cf9477adb3a0d27e674b99f741a8a`.
It supersedes older host-identity/combined-observation and outer-Close outcomes
below; unchanged algorithm/component anchors retain their original bounds.

Scoped rejection/fault evidence uses the current linked owners and public
vectors; unchanged selected-host alignment references retain
`69b1cfe3c85d296463da0def3e80eaca0ddcd0a7`; Close references retain explicit
`aad23335c219a576d69133cee883bbd63e40b68a` anchors. Unchanged component references
retain their explicit `e2a47b6c631c974aa0135e81c3a7e3a439980ded` anchors.
Native SF is
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`. It is not a delivery log.
[Current semantics](classic-v1-runtime.md) owns behavior;
[pending work](sf-v1-semantic-alignment-plan.md) owns future obligations;
[provenance](sf-v1-port-provenance.md) owns exact native source/test ancestry,
representation changes and licenses. [Testing](testing.md) owns commands.

**Evidence classes:** inspected source; inspected existing actual-component
test; selected public-host/factory/controller test; application-file/process
test; native source/test expectation. Targeted rejection/local-fault and Close/access regressions were
executed; other cited bodies do not imply additional runs. Historical “Open,” removal of the old engine,
test discovery or a private adapter pass establishes neither current failure
nor closure. “Supported” below is bounded to the cited behavior, not complete
compatibility. “Unverified” is not “defective.”

Bounded custom-native controller formation evidence is anchored to
`366b362ce4e1c9fdc6aaf0fe64e6be78931e96a3`: the actual PostgreSQL
[controller witnesses](../../../examples/postgres/src/public_controller_tests.rs)
and runtime contract negatives were executed with the authorized PG smoke and
targeted runtime/controller checks. This does not extend the older evidence
above to native binary execution or unavailable downstream engines.
Current linked witnesses additionally bound overlapping native observer
ownership, serialized pre-effect invalidation and graceful Close observation;
the formation anchor is not a blanket claim for every interleaving.

## Current semantic and gap dispositions

Stable historical IDs preserve the original issue's meaning without retaining
obsolete traces as today's narrative. Every row names current evidence and its
limit. Detailed native ancestry lives in the provenance register, not a duplicate
scenario oracle.

| ID / original issue | Current disposition | Direct current evidence / limit |
|---|---|---|
| **SF-01** — visible KV apply before read fence | Supported service-isolated visibility; old mechanism superseded | [KV:195–376][kv], [test:554–611][kv-read]. Native quorum does not expose speculative primary state/client success; secondary normal reads denied. Not per-apply host read denial. |
| **SF-02** — KV bypasses write denial | Supported selected write admission; historical Open label stale | [adapter:421–448][admission], [Revoke:374–380][revoke], [host-gate test:11–145][gate-test], [public raw-fence assertion:4413–4422][raw-fence]. Access sample/assignment share Revoke gate. No separately rerun exact HTTP-only legacy trace. |
| **SF-03** — PG ignores partition denial | Supported periodic live-status policy | [PG:626–731][pg-access], [tests:3–158][pg-access-test]. Not instantaneous/per-statement revocation; isolated secondary status is not a stable-secondary public-host grant. |
| **SF-04** — swap capability mismatch | Supported capability/mode binding | [controller:445–470][fallback], [public wait assertions:4396–4412][capability], [swap:4360–4468][swap]. True/WriteQuorum and false/All are distinct real-bundle cases; crossed metadata rejects. |
| **SF-05** — demotion precedes final catch-up | Supported source configuration/role order and selected staged access | [program:20–39][program], [public:4423–4451][swap], [selected stage barriers:2030–2159][aligned-swap]. Final wait returns before paired role transition; read denial is immediate before the outcome receipt. |
| **SF-06** — Revoke invokes obstructing synthetic role callback | Supported access-only Revoke; public child failure and actual containment are separate | [Current common contract and actual V1/PG dispatch](engine-neutral-hosting.md). A child Close error stays a failed public result after successful cleanup; failed/lost native joining never publishes positive containment. Earlier outer-success anchors are historical, not the current policy. |
| **SF-07** — PG idle Remove globally drains | Supported completed-idle retirement | [PG:1919–1952][pg-remove], [test:607–656][pg-remove-test]. Same process/healthy SQL survives idle Remove; voters must first leave configuration. |
| **SF-08** — retired PG grants leak slots | Supported selected managed-slot reclamation | [slots:212–258][slots], [dirty cleanup:784–789][dirty], [cancel-without-retry test:660–688][pg-remove-test]. Slot absence releases that WAL pin, not a disk-growth SLA. |
| **SF-09** — stopped PG progress regresses | Supported orderly terminal observation only | [PG:567–617][pg-stop], [1507–1525][pg-progress], [test:163–249][pg-stop-test]. Primary certificate and secondary receive/replay differ; unobserved racing SQL, Abort snapshot and arbitrary survivor election remain unverified. |
| **SF-10** — quiet snapshot needs nonexistent original record | Supported actual copy/session boundary | [public:1661–1692][quiet], [1800–1840][suffix], [CopySender:218–293][copy]. Empty/nonzero snapshots and suffixes use current gates, not old durable receipts. |
| **SF-11** — prefix skipped before ambiguous suffix | Supported current ordered-prefix safety; exact historical shape unverified | [current prefix:146–214][prefix], [gap test:151–172][modes]. Current witness preserves acknowledged6–10 before ambiguous11; it is not the old `(11,10)` reconciliation against peer5 or its removed negative controls. |
| **SF-12** — dispensable rejection response order fails quorum | Supported **bounded current scoped ordering**; original finding belongs to removed engine | [Owner:18–155][rejection-owner] fails remote idle Build independently of healthy progress in both orders. [Public:57–320][rejection-public] forwards an actual configuration-denied C listener response and observes source consumption before/after real B apply, marked catch-up and service commit; a same-facade query/retained-native-future handshake establishes pending catch-up before apply. Required PC/CC/marked/All controls remain distinct. Not the old typed proof-loop error, every rejection class or native executable equivalence; no current dispensable-order defect reproduced. |
| **SF-13** — PG ignores marked successor | Silent success removed; marked mode **explicitly unsupported** | [PG:1476–1504][pg-wait], [tests:253–368][pg-wait-test]. Marked-successor WriteQuorum rejects before waiting. Ordinary unmarked WriteQuorum/All work; false-capability swap falls back to All. |
| **SF-14** — disconnected service delivery | Supported real receiver delivery/ACK | [production bridge:341–378][bridge], [public:1696–1796][ack]. Persisted service ACK holds actual completion; no old progress-bearing verification/automatic-apply contract. |
| **SF-15** — KV trait Close omits joins | Supported retained trait-Close consumer drainage | [service:200–218][kv-close], [372–384][kv-tasks], [test:478–532][kv-close-test]. Cancel/retry retains joins/errors; not inferred from host shutdown. |
| **SF-16** — retained PG gateway survives Abort | Supported retained owner containment | [PG:500–516][pg-abort], [tests:940–1050][pg-abort-test]. Actual process/socket/listener/grant observations without Close/Drop; not synchronous arbitrary-work join or latency guarantee. |
| **SF-17** — port-zero settings round-trip conflicts | Supported unchanged effective endpoint; wider settings deferred | [settings:390–438][effective], [production:493–499][effective-bridge], [test:451–502][effective-test]. Unresolved/original port0 and different endpoint reject; not getter/rebind/P7 completion. |
| **SF-18** — copy never EOF/single-take handles | Supported shared terminal copy and sequential consumption | [getter test:774–829][streams], [fixture:448–505][fixture-stream], [public:1861–1939][external]. Closed/error versus EOF remains distinct; no arbitrary in-place build reset or all getter orders claimed. |
| **SF-19** — source supplies own copy context | Supported persisted target-origin exchange | [establishment:473–478][context-mode], [738–757][context-source], [test:1013–1091][context-test]. Distinct nonempty proof is component-level; external public provider uses empty context. Volatile omission is deliberate. |
| **SF-20** — no early assignment/engine owns local commit | Supported early LSN and service commitment | [production:473–491][assignment], [external journey:1861–1939][external], [KV:195–376][kv], [SQLite:1731–1801][sqlite]. Independent provider needs no helper hook; client success is later than quorum. |
| **SF-21** — progress-bearing always-explicit ACK | Supported parameterless ACK and core mode split; public policy expansion deferred | [public stream:19–51][stream-api], [modes:121–172][modes], [production:121–158][policy]. Persisted explicit/volatile optimistic selected; volatile required-ACK core tests are not a public setting. |
| **SF-D01** — capability is committed tail | Supported role-specific native queue capability | [primary getter:322–331][progress], [secondary:369–385][secondary-progress], [queue heads:144–157][heads]. Primary first committed vs secondary first retained completed; zero without that range, not universal tail/floor. |
| **SF-D02** — primary query overpromises application durability | Current contract is native queue progress, not old durable-commit promise | [primary separation:16–71][primary-query], [secondary separation:76–139][secondary-query]. Quorum may precede local commit; receive may precede service ACK. Historical committed-cache fix is not today's semantics or a complete election proof. |

The unnumbered sentinel clarification remains: `INVALID_LSN` is input unknown
progress, not empty history or cross-identity reusable state
([description:153–163][sentinel]).

### Additional selected behavior and evidence boundaries

| Category | Selected-source evidence | Disposition |
|---|---|---|
| Core — outer Close | [Current common executor/terminal map](engine-neutral-hosting.md#pinned-message--action--state-mapping), actual V1/PG hosted terminal witnesses vs [native outer policy][native-close] | Intentional local divergence: retained failed child result after positive actual containment, rather than native success-shaped outer Close. Abort-both/skip-later-graceful remains deliberate; no generic application-task join claim. |
| Core — staged access | [selected policy:209–300][aligned-access], [public barriers:144–2159][aligned-access-tests] vs [native selector:288–381][aligned-native-selector] | Supported bounded same-primary/promotion/demotion stage policy, primary-context validation, same-host repromotion/reconciliation, cancellation/replay and compatible final release. Missing progress yields explicit unavailable facts; descriptor availability is not remote liveness. Broader stage/interleaving qualification remains unverified. |
| Core — primary-local Build fault entry | [Manager:793–805,1028–1045][local-fault-entry], [owner:157–230][rejection-owner], [public:333–392][rejection-public] | New assignment/catch-up reject latched GetCopyState/EndGetNext faults. Regression negative controls demonstrated cached-ready catch-up success and new owner assignment before the gate. This is a separate local fault-entry mismatch, not SF-12 remote ordering. Pending operations and Close retain separate policies. |
| Source-contract wording | [catch-up Rustdoc][catchup-doc], [epoch Rustdoc][epoch-doc] and [manager:975–1024][manager], [292–302][epoch] | Aligned selected V1 contract: moving All, captured-boundary/current-quorum/live-marked WriteQuorum, and issued-tail primary epoch callbacks. Secondary stream boundaries remain distinct; catch-up/epoch boundaries do not certify provider commitment or durability. Protected signatures and runtime behavior are unchanged. |
| Custom native authority/observations | [Supported companion](replicator-boundary.md#custom-controller-integration), [actual PG controller](../../../examples/postgres/src/public_controller_tests.rs), [real-host contract negatives](../../../kuberic-runtime/tests/support/custom_control.rs) | Supported bounded independent-engine Build authority, coherent installed/idle observations, exact live native replay/invalidation and real controller joint/current-only admission with SQL fencing. Local authority/fresh-initialization policy is not an SF public protocol or native FM algorithm. No unpublished MySQL trace, automatic arbitrary failed-Build reformation, native binary differential or production-readiness claim. |
| Common hosting abstraction | [Common coordinator, action admission and separate evidence](engine-neutral-hosting.md), [actual V1 dispatch](../../../kuberic-level-tests/src/in_process/journeys.rs#L1), [independent PG dispatch](../../../examples/postgres/src/public_controller_tests.rs#L186) | Supported bounded source-guided RAP/FUP/common-proxy responsibility port. Explicit selected-callback ownership replaces engine-kind authority/lifetime decisions; native diagnostics remain optional. Installed host configuration, host remote Build state and actual idle samples are independent. Exact fences, retained joins, current-only renewal synchronization and replay-safe compare-revoke remain local adaptations, not full RA/FM/native executable equivalence. |
| Security | [HTTP:177–227][http], [bearer:728–766][auth] | Authenticated but no TLS; ordinary deployment constraints remain. Missing native credential API is not accepted-but-unenforced settings. |
| Qualification | [public external provider:1861–1939][external], [real SQLite:1731–1801][sqlite], component/native ancestry | Actual Rust/application evidence is not native binary differential, kernel power-loss or production readiness. |

## Current owner and source-family map

Destinations below are under `kuberic-runtime/src/replicator/` unless named.
F IDs retain source-family traceability, not completion percentages. Exact
native ranges/test cases and substitutions are in the corresponding
[provenance sections](sf-v1-port-provenance.md#component-material-register).

| Family | Current responsibility / destination | Selected exposure and limit |
|---|---|---|
| F01 facade/role | `v1/role.rs`, `service_adapter.rs`, `service_adapter/facade.rs` | Public default composition selects them ([composition:127–227][composition]); private internals are not exposed as API. |
| F02 primary/quorum | `v1/primary.rs`, `replica_manager.rs`, `replication_queue_manager.rs` | Assignment, PC/CC, catch-up and build ownership; [manager:846–1024][manager]. |
| F03 queue | `v1/operation_queue.rs` | Independent heads/retention/admission, not a plain channel; [heads:144–157][heads]. |
| F04 secondary | `v1/secondary.rs`, receivers, role handoff | Receive/ACK/drain distinctions; [receiver:369–385][secondary-progress]. |
| F05 operation/data/stream | `stream.rs`, `v1/received_operation.rs`, `operation_stream.rs`, `buffers.rs` | Public shared streams and parameterless ACK; selected raw buffer policy, private segmented codec not public parity ([stream:19–51][stream-api], [policy:121–158][policy]). |
| F06 sender/session/ACK | `v1/sender.rs`, `session.rs`, `ack.rs`, `async_runtime.rs` | Ordered effects/retries and separate receive/apply; fixed production policies ([composition:127–227][composition]). |
| F07 copy/build | `v1/build_idle.rs`, establishment/enumerator/sender/context receiver | Target context and actual EOF target/gates, not fixed snapshot-only success ([copy:218–293][copy], [context-source:738–757][context-source]). |
| F08 carrier/demux | `v1/transport.rs`, `ingress.rs`, `service_adapter/production_transport.rs`; `host/native_transport.rs` | Tonic/bearer/exact host authority, not native platform transport/wire ([dispatch:365–400][dispatch]). |
| F09 recovery/access/lifecycle | `v1/role.rs`; `host/hosting.rs`, `selected_access.rs`, `public_lifecycle_program.rs` | Selected operation-owned stage policy and retained Close cleanup are actual production paths; private `v1_lifecycle.rs` ancestry alone is not the evidence ([selected:209–300][aligned-access], [Close:550–638][aligned-close]). |
| F10 settings/reporting | `mod.rs`, fixed production policies, public partition methods | Partial/deferred full native public exposure ([settings:275–283][settings], [partition:1036–1064][partition]). |
| F11 async/error/drain | Retained facade/transport/provider tasks and service committers | Rust future/task/waker substitutions; direct Close, Remove and separate join are not interchangeable ([facade test:351–387][join]). |

## Original target and acceptance-family disposition

The original P1–P8 program remains the scope provenance, **not today's cursor**:

| Original family | Current disposition |
|---|---|
| P1 source/contracts/notices | Pin, provenance and protected compiler fixtures retained; not an executable native oracle. |
| P2 queue/session/async/carrier | Real private components selected in production composition; no longer “unselected P2.” |
| P3 primary/secondary/copy/role/recovery | Real selected owners and bounded regressions, not exhaustive native algorithm-domain proof. |
| P4 service/host adapters | Assignment/stream/commit bridges plus bounded selected-host stage/Close alignment; not wholesale selection of the private host or native RA/FM. |
| P5 production selection | Sole default factory selects classic owners; selection is not every alignment exit criterion. |
| P6 custom/application closure | Current bounded PG/KV/settings support above; marked PG WriteQuorum deliberately rejects. |
| P7 capabilities/extensions | Broad expansion deferred **within original target**, not implemented or excluded. Existing bearer/persistence/basic reports remain present. |
| P8 documentation/qualification | Current docs/dispositions supplied; broad executable/native differential/power-loss qualification unverified. |

The original contract families remain represented as follows. The FR IDs here
belong to the original alignment target, not local rewrite artifacts:

| Original contract | Current reference / outstanding part |
|---|---|
| FR-001 assignment/completion/cancel; FR-002 service commit | [Admission and commitment](classic-v1-runtime.md#admission-assignment-and-application-commitment); SF-20 |
| FR-003 ACK modes; FR-004 copy/context/build | [Streams](classic-v1-runtime.md#shared-streams-ack-and-terminal-states), [Copy](classic-v1-runtime.md#copy-and-build); SF-18/19/21, public qualification gaps |
| FR-005 progress/epoch/reset; FR-006 All/WriteQuorum/PC/CC | [Quorum and progress](classic-v1-runtime.md#quorum-catch-up-and-progress); SF-D01/D02, SF-12 qualification |
| FR-007 access/callbacks; FR-008 Close/error/Abort/retirement | [Host/lifecycle](classic-v1-runtime.md#host-access-lifecycle-and-retirement); bounded selected stage/Close alignment with explicit remaining ownership/qualification limits |
| FR-009 settings/reporting; FR-015 actual exposure/provenance | [Policies/security](classic-v1-runtime.md#production-policies-and-security), [deferred P7](sf-v1-semantic-alignment-plan.md#deferred-broad-p7--still-in-the-original-target) and provenance |
| FR-010 identity/history/provider; FR-011 protected types/lossless data | [Selection](classic-v1-runtime.md#selection-and-ownership), [authority](classic-v1-runtime.md#production-policies-and-security); public external provider supported, full public buffer-policy exposure deferred |
| FR-012 reassess historical findings | This matrix, not inherited aggregate Fixed/Open counts |
| FR-013 complete responsibility target; FR-014 async/tonic lifetime/order | Owner families above; scoped evidence and remaining qualification, not a declaration of full closure |

Source-derived acceptance IDs retain ancestry without duplicating old fixed
numeric oracle tables:

| Acceptance family | IDs | Present evidence/disposition |
|---|---|---|
| Admission / ordered queue | A1, Q1–Q4 | Assignment/queue/ACK/progress above; native named cases retained in provenance |
| Quorum / catch-up | M1–M5 | Real manager predicates plus bounded actual-owner/public both-order rejection evidence; not universal rejection/native-execution qualification |
| Secondary / callback modes | S1–S3 | Shared stream and ACK tests; mode-specific Close/fault limits stay distinct |
| Sender / session / ACK | R1–R3 | Actual component ancestry; fixed public policy is not every native default |
| Facade / copy / settings | C1–C6 | Current copy/context/gates and explicit unsupported/deferred exposure |
| Routing / data | T1–T2, D1 | Actual authority/carrier and LSN identity; no native wire parity/public segmented policy claim |
| Role / recovery / epoch | L1–L3, E1–E2 | Actual queue/role recovery and contained outer Close; no blanket crash-cut/native execution claim |
| Access / reporting | H1–H3 | Bounded selected stage policy supported; broad scoped-report capability deferred, not complete |

## Consolidation map

Git history is the historical record; no archive or redirect-stub collection is
maintained. Former filenames here are plain text, not live navigation targets.

| Former document/material | Current destination |
|---|---|
| `service-fabric-api-semantics.md` | `classic-v1-runtime.md`: current behavioral topics |
| `service-fabric-alignment.md` | Runtime selection/scope and pending-items plan |
| `service-fabric-contract-audit.md` | This current disposition matrix; old audit prose removed |
| `sf-v1-p6-closure.md` | Runtime PG/KV/settings behavior and these qualified audit rows |
| Historical `sf-v1-semantic-alignment-plan.md` phase cursor | Rewritten current pending-items plan, original-target disposition above |
| `sf-v1-port-ledger.md` slice/status/oracle repetition | Current evidence ledger here; exact native ancestry in provenance |
| `sf-v1-port-provenance.md` progress/run histories | Component material/ancestry register, substitutions and exclusions |
| `testing.md` repeated slice/run narratives | Current commands/tiers with links to evidence and provenance |

[aligned-close]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/src/host/public_lifecycle_program.rs#L550-L638
[rejection-owner]: ../../../kuberic-runtime/src/replicator/v1/primary/tests/rejection_order.rs#L18-L230
[rejection-public]: ../../../kuberic-runtime/tests/public_v1_recovery/rejection_order.rs#L57-L392
[local-fault-entry]: ../../../kuberic-runtime/src/replicator/v1/replica_manager.rs#L793-L1045
[aligned-close-tests]: https://github.com/youyuanwu/kuberic/blob/aad23335c219a576d69133cee883bbd63e40b68a/kuberic-runtime/tests/public_v1_recovery.rs#L3114-L3505
[aligned-access]: https://github.com/youyuanwu/kuberic/blob/69b1cfe3c85d296463da0def3e80eaca0ddcd0a7/kuberic-runtime/src/host/selected_access.rs#L209-L300
[aligned-access-tests]: https://github.com/youyuanwu/kuberic/blob/69b1cfe3c85d296463da0def3e80eaca0ddcd0a7/kuberic-runtime/tests/support/staged_access.rs#L144-L2159
[aligned-swap]: https://github.com/youyuanwu/kuberic/blob/69b1cfe3c85d296463da0def3e80eaca0ddcd0a7/kuberic-runtime/tests/support/staged_access.rs#L2030-L2159
[aligned-native-selector]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp#L288-L381
[kv]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/kvstore2/src/v1.rs#L195-L376
[kv-read]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/kvstore2/tests/read_fencing.rs#L554-L611
[admission]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter.rs#L421-L448
[revoke]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/hosting.rs#L374-L380
[gate-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/tests/host/admission.rs#L11-L145
[raw-fence]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L4413-L4422
[pg-access]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L626-L731
[pg-access-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1_p6_tests.rs#L3-L158
[fallback]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-controller/src/evaluator/public_v1_topology.rs#L445-L470
[capability]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L4396-L4412
[swap]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L4360-L4468
[program]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/host/public_lifecycle_program.rs#L14-L44
[pg-remove]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L1919-L1952
[pg-remove-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1_tests.rs#L607-L688
[slots]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/native.rs#L212-L258
[dirty]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L784-L789
[pg-stop]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L567-L617
[pg-progress]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L1507-L1525
[pg-stop-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1_p6_tests.rs#L163-L249
[quiet]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L1661-L1692
[suffix]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L1800-L1840
[copy]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/copy_sender.rs#L218-L293
[prefix]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/support/read_fencing.rs#L146-L214
[modes]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/secondary_receiver/tests.rs#L121-L172
[manager]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replica_manager.rs#L846-L1024
[minority]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L2302-L2433
[marked]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replica_manager/tests.rs#L690-L719
[pg-wait]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L1476-L1504
[pg-wait-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1_p6_tests.rs#L253-L368
[bridge]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L341-L378
[ack]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/public_v1_recovery.rs#L1696-L1796
[kv-close]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/kvstore2/src/service.rs#L200-L218
[kv-tasks]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/kvstore2/src/service.rs#L372-L384
[kv-close-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/kvstore2/tests/read_fencing.rs#L478-L532
[pg-abort]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1.rs#L500-L516
[pg-abort-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/examples/postgres/src/public_v1_tests.rs#L940-L1050
[effective]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/mod.rs#L390-L438
[effective-bridge]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L493-L499
[effective-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/classic_v1_production.rs#L451-L502
[streams]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/tests.rs#L774-L829
[fixture-stream]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/scripts/fixtures/classic-v1-provider/src/lib.rs#L448-L505
[external]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/classic_v1_production.rs#L1861-L1939
[context-mode]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/copy_establishment.rs#L473-L478
[context-source]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/copy_establishment.rs#L738-L757
[context-test]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/copy_establishment/tests.rs#L1013-L1091
[assignment]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L473-L491
[sqlite]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/classic_v1_production.rs#L1731-L1801
[stream-api]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/stream.rs#L19-L51
[policy]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L121-L158
[progress]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replication_queue_manager.rs#L322-L331
[secondary-progress]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/secondary_receiver.rs#L369-L385
[heads]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/operation_queue.rs#L144-L157
[primary-query]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/support/read_fencing.rs#L16-L71
[secondary-query]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/tests/support/read_fencing.rs#L76-L139
[sentinel]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/mod.rs#L153-L163
[native-close]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.CloseAsyncOperation.cpp#L94-L275
[catchup-doc]: ../../../kuberic-runtime/src/replicator/traits.rs#L107
[epoch-doc]: ../../../kuberic-runtime/src/traits.rs#L53
[epoch]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/replica_manager.rs#L292-L302
[http]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L177-L227
[auth]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L728-L766
[composition]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production.rs#L127-L227
[dispatch]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/production_transport.rs#L365-L400
[settings]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/mod.rs#L275-L283
[partition]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/mod.rs#L1036-L1064
[join]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/v1/service_adapter/tests/facade.rs#L351-L387
