# SF V1 Port Foundation Ledger

**P1 foundation only; no production behavior change.** Native pin:
[`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`][native].
This ledger implements the foundation deliverables in the
[faithful-port roadmap](sf-v1-semantic-alignment-plan.md#dependency-ordered-implementation-program).
The current bespoke engine is the migration/safety baseline, not the native
specification. All `v1/` destinations below are **proposed future paths** under
`kuberic-runtime/src/replicator/`; none is activated or implemented by P1.

## Evidence and status

These statuses apply to **every family, contract and scenario row** below:

| Dimension | P1 status |
|---|---|
| Native production source | source-reviewed at the immutable pin |
| Selected native tests | derived expectations where explicitly named; otherwise production-derived synthetic inputs |
| Port implementation/review | **pending P2+ / no component implementation to review** |
| Native binary/test execution | **not run** |
| Ported component execution / native differential comparison | **not implemented / not run** |
| P1 fixture execution | ordinary expectation-validator tests, not native behavior execution |
| Current-engine execution | only separately named existing Rust witnesses, below |
| KSF | existing structural/current-contract evidence, not native parity |

The [test-private data](../../../kuberic-runtime/tests/support/sf_v1_vectors.rs)
contains **11 families, 15 contracts, 33 scenario vectors, 29 capability/settings
cases and 8 current-evidence entries**. The
[ordinary oracle](../../../kuberic-runtime/tests/sf_v1_foundation.rs) has seven
tests: structural completeness/evidence separation; direct catch-up predicate
slices; selected queue/sender expected assertions; ACK/default/exposure policies;
copy expected gates; and buffer-shape distinctions. Fixed traces are expectations,
not an independently implemented native engine. Not every inventoried transition
has an executable predicate: role/epoch/full lifecycle, transport, retry scheduling
and most capability behavior remain expected-vector inventory pending P2+.
No timing sleeps, native checkout dependency, wire codec or queue simulator is
needed to execute these fixtures, including from an archive.

The [provenance register](sf-v1-port-provenance.md) identifies selected
translations, changes and notices. Source citations below are pinned. `R/`
means `src/prod/src/Reliability/Replication/` in that repository; exact ranges
also live alongside each fixture, including additional supporting sources.

## Source families, transitions and substitutions

| ID / native family and source | Transitions/responsibility | Future Rust destination | Allowed representation substitution | Scenarios |
|---|---|---|---|---|
| F01 facade/factory/ReplicatorState ([state:39–126][state], [QI:12–35][qi]) | initial/transitional/primary/secondary/faulted/closed; role admission | `v1/replicator.rs`, `state.rs`, existing facade | COM/refcounts → owned Rust, not altered role outcomes | A1, C1, L1 |
| F02 PrimaryReplicator/ReplicaManager/ReplicationQueueManager ([manager:349–421][manager], [queue-manager:261–408][qm]) | CC/PC/idle sessions; enqueue/assignment/completion; quorum/config capture | `v1/primary.rs`, `replica_manager.rs` | owned futures replace Begin/End, not primary ACK, configured W or B/Q/M arithmetic | A1, M1–M5, E1 |
| F03 OperationQueue ([queue:677–737,990–1047][queue], [heads:62–87][qh]) | gap/order, retention, count/byte admission, commit/complete/trim/reset/transfer | `v1/operation_queue.rs` | owned storage instead of ring layout, not a plain channel or merged cursors | Q1–Q4, L2 |
| F04 SecondaryReplicator/receivers ([secondary:165–193,951–1003][secondary], [receiver:632–740][receiver]) | reorder/dispatch/completion, fixed copy/minimum replication epoch, drains | `v1/secondary.rs`, `receiver.rs` | async queues/executor; preserve implicit dispatch vs service completion and receiver lifetime | S1–S3, E2 |
| F05 operation/data/stream/provider ([stream:24–80][copy-receiver], [callback:167–224][callback], [IDL:636–683][idl]) | shared getters, data/EOF/EOS, late callback states, provider enumeration | existing protected facade + `v1` adapters | owned callbacks; protected Bytes/boxed stream/i64 unchanged; explicit segmented adapter later | D1, S2–S3, C1–C2 |
| F06 ReplicationSession/RemoteSession/ReliableOperationSender/AckSender ([sender:548–755][sender], [ACK order:585–627][remote], [batch:64–112,174–208][ack]) | incarnation, separate receive/apply, retry/window/RequestAck, final ACK | `v1/session.rs`, `sender.rs`, `ack.rs` | tasks/injected clocks, not deleting native retry/flow control because HTTP/2 is reliable | R1–R3, T2 |
| F07 CopySender/CopyAsyncOperation/context/establishment ([copy:171–254][copy], [build:111–166][build], [retarget:524–579][copy-sender]) | target context, serial polls, one parked item, EOF target and pending retarget | `v1/copy.rs`, provider/stream adapters | bounded async queues/futures, not source-local context or a fixed snapshot-only gate | C2–C5 |
| F08 transport/demux/headers ([send:880–937][transport], [session lookup:632–683][primary], [header:25–42][header]) | logical messages, full identity, two epochs, sequence spaces, expiry | `v1/transport.rs`, future replication proto | tonic carrier, no native wire compatibility; accepted send is not service ACK | T1–T2, D1 |
| F09 provider epoch/recovery/RA boundary ([epoch:31–90][epoch], [reset:82–126][loss], [status:34–145][status]) | issued epoch boundary, queue recovery/transfer, lifecycle access/direct vs outer Close | `v1` + existing host/controller adapters | no RA/FM/placement port; optional provider durability is not native queue progress | E1–E2, L2–L3, H1–H2 |
| F10 settings/actual extensions/partition reports ([settings:490–532,762–808][com], [public defaults:140–177][defaults], [partition:208–350][partition]) | effective settings/update validation, persisted declaration, faults/load/move/health | existing facade/settings/partition adapters, P4/P7 | Rust types; preserve public defaults, unsupported results and credential enforcement | C1, C6, H3 |
| F11 async lifetime/diagnostics ([drain:56–102][drain], [health:62–125][health]) | retain owners through cancel/drain, outside-lock dispatch, batched/cleared warnings | Rust async ownership/clock + host reporting | no Common/Transport/KTL/Boost copy; preserve algorithm-affecting policy, not platform plumbing | R3, C3, H3 |

## Contract rows

These stable FR IDs retain the requirements of the approved earlier
specification; the public roadmap expresses them narratively rather than as
numbered rows. Meanings are included here so no ignored artifact is required.
Each row inherits the source/destination/substitution and pending/run status of
its named families, not an assertion that the contract is implemented.

| Contract / native target | Family → destination and substitution rule above | Concrete scenarios / current evidence |
|---|---|---|
| FR-001 successful admission/LSN separate from completion; advisory cancellation | F02/F11 (owned futures, not rollback) | A1, C3; held primary ACK/commit witness |
| FR-002 service-owned preparation, ordered local commit, visibility/client reply | F02/F09 (queue ≠ provider persistence) | A1; current primary-local commit counterexample |
| FR-003 persisted/required vs volatile implicit ACK; contiguous completion/late scope | F03/F04/F06 (separate cursors) | Q1, S1, S3, R1; held secondary ACK witness |
| FR-004 target context/EOF/receiver lifetime/captured and retargeted build gates | F05/F07 (shared receiver/serial provider) | S2, C2–C5, L1 |
| FR-005 role progress/retained capability/query/reset/epoch | F03/F09 (distinct heads and provider evidence) | Q3–Q4, L2, E1–E2 |
| FR-006 live All, captured WriteQuorum, successor and CC/PC queue constraints | F02 (exact equations) | M1–M5 |
| FR-007 classic access/callback phases; service isolation before native grants | F01/F09 (host adapter, not RA port) | H1–H2, L1 |
| FR-008 direct vs outer Close/error/Abort/cancel/retirement | F05/F09/F11 (owned drainage) | S3, C3, L3; current outer Close witness |
| FR-009 persisted declaration/settings/specific quorum/fault/EOS/scoped reports | F10 (effective defaults/validation preserved) | C1, C6, H3; cases below |
| FR-010 identity/history/provider guarantees; no substitute semantic gates | F08/F09 (tonic fencing + separate evidence) | T1, L2; helper-free hosted service remains P5 |
| FR-011 protected resolved interfaces and lossless buffer representation | F05/F08 (Bytes/boxed Result<Bytes>/i64 fixed) | D1; existing external compiler fixture strengthened; codec pending P2 |
| FR-012 retain historical audit evidence; no closure from planning/ledger | F02/F04/F09 (current vs target separate) | A1, S1, L3 counterexamples; audit remains 7 fixed/14 open + SF-D01; historical SF-D02 fixed |
| FR-013 faithful source-guided complete responsibility port | F01–F11 (all substitutions bounded) | all 33 inventoried vectors; components pending |
| FR-014 Rust async/tonic retain lifetime/order/retry/epochs/ACK/errors | F06/F08/F11 (execution/carrier only) | R1–R3, T1–T2, D1 |
| FR-015 provenance and actual native exposure, not every SDK declaration | F05/F10 (facade boundary) | C1, C6, H3; selected-material register |

## Deterministic scenario inventory

**D** = selected native test assertions translated/simplified; **S** =
production-source-derived small scenario. D does not mean the entire upstream
test/harness was ported. All are source-reviewed expected vectors; sources and
named native cases are preserved in the linked Rust data and provenance table.

| ID / derivation | Fixed input/actions → expected native observations | Native evidence |
|---|---|---|
| A1 D/S | progress10; enqueue11 accepted with quorum held → assigned11 before completion; failed full enqueue12 leaves next12. Secondary Begin S_OK / End NotPrimary. | [queue-manager:369–408][qm], [PublicApi.Test:454–504][api-test] |
| Q1 D/S | fixed receive2,1 then ACK2,1 → contiguous receive0,2; completed0 until ACK1, then2; no gap skip. Reduced from randomized test. | [queue:677–737,990–1047][queue], [queue tests:1049–1095][queue-test] |
| Q2 D/S | start10, initial4/max8/memory600; four100-byte items, complete one retained → bytes400/retained100; add301 rejected without mutation; add100 →500. Source-derived neighbor: add300 admitted after evicting completed100 →600 (sufficient count growth). | [queue tests:435–461][queue-test], [queue:256–318][queue] |
| Q3 D/S | complete before commit false, after commit true; separately progress100/no retained history → capability0. | [queue tests:607–615][queue-test], [heads:76–87][qh] |
| Q4 D/S | commit1..8, history4; complete0,1,4,5,8 → first completed sequence0,1,1,2,5; tail8. Duplicate does not grow count/bytes. | [queue tests:867–901][queue-test], [queue:371–399][queue] |
| M1 D | W2 remotes[5,6] → quorum6/reclaim5; W3 [2,3,5] →3/2. Too few configured replicas stays pending. | [manager tests:154–220][manager-test] |
| M2 S | All T12/Acc11 false; Acc12 true; another evaluation after T13/Acc12 false until13. Completed waits do not reopen. | [catch-up:43–59][catchup] |
| M3 S | B10/Qcc11/M10 false despite M≥B; M11 true; unmarked only Qcc≥B with enough replicas. Ordinary tail12 does not recapture B. | [catch-up:62–104][catchup], [capture:261–272][qm] |
| M4 S | CC W1/no remote candidate normalized to tail12; PC W1/lone remote7 constrains normal commitment7. | [manager:2471–2524][manager], [queue-manager:319–357][qm] |
| M5 S | B10, candidates commit12/complete11; catch-up-config and AllowMultipleQuorumSet=false cap10/10; true/inactive removes this cap. Missing PC blocks. Configuration at tail13 captures B13. | [manager:1278–1333,2328–2362][manager] |
| S1 S | (persisted,require) FF implicit, FT/TF/TT explicit; persisted receive1 gives current1/complete0 before service ACK; implicit completes at dispatch before dequeue. | [secondary:165–193][secondary], [receiver:177–202,632–740][receiver] |
| S2 S | two getters share consumed queue; terminal reacquisition stays terminal; demotion copyDone gives closed empty copy. | [copy receiver:24–80][copy-receiver] |
| S3 S | retire normal NotCalled callback cancels it; late ACK cannot advance retired receiver; repeated completed ACK InvalidOperation; EOS callback differs. | [callback:167–224][callback] |
| R1 D | add1,2,3; ACK(receive,apply) (1,0),(2,0); add4; (2,1),(3,2),(4,4) → pending[2,3],[3],[3,4],[4],[] and timer off. | [sender test:102–143][sender-test] |
| R2 S | active sender window8/max8/noAckSinceLastCallback=true; retry→4; advancing receive ACK doubles4→8; duplicate leaves8; receive4/apply2/no pending send→RequestAck, not quorum. | [sender:548–579,636–679,732–755][sender] |
| R3 S | combined ACK effects ordered sender progress → build gate → progress callback → copy ACK; batch15ms<retry5s; Close flushes final ACK. | [remote:585–611][remote], [ACK:64–112,174–208][ack] |
| C1 S | actual QI/settings/fault/exclusion outcomes in next table, not SDK assumptions. | [QI:12–35][qi], [Com:1002–1050][com], [stream:66–85][stream] |
| C2 S | provider [empty bytes,x,EOF] →3 polls,2 items,1 terminal EOF, no poll4; persisted target context to source, volatile omits callback. | [enum:108–121,182–195][enumerator], [copy:171–211][copy], [secondary:982–1003][secondary] |
| C3 D/S | native15/15/4 cancel-at12 case reduced to full queue + one parked item; no additional poll until room/termination; cancel is OperationCanceled, not EOF. Copy count64/1024; no maximum-memory limit. | [copy test:418–449][copy-test], [copy:42–49,242–254][copy], [copy receiver:388–395][copy-receiver] |
| C4 S | prefix10/EOF-issued12; final-copy ACK and apply11 pending, apply12 satisfies replication gate; quiet baseline10/tail10 needs no invented op10 record. | [build:111–144][build], [copy sender:524–579][copy-sender] |
| C5 S | pending LSNSet target12; freshness committed13→13; pressure issued15→15; lower/equal refused; ReplCompleted cannot rise. | [copy sender:555–579][copy-sender], [manager:745–774,1983–2022][manager] |
| C6 S | public defaults/update cases below; retry5s→6s specified public update InvalidArgument; legacy initial128 sets both roles, memory600 without legacy max sets counts0/0 vs max1024 flag gives1024/1024; later EX3 overrides per role before comparison. Credentials validated/updated. Config-store dynamic updates differ. | [Com:490–582,639–808][com], [defaults:136–207][defaults], [public config:12–17][reconfig] |
| T1 D/S | wrong endpoint drops; same replica wrong incarnation cannot update valid session; matching identity still needs role/epoch checks. Sockets/sleep removed. | [transport test:594–617][transport-test], [primary:632–683][primary] |
| T2 S | repl/copy expiry5s; StartCopy/RequestAck unbounded; queue-full stops batch; too-large faults; accepted send≠ACK; slow idle notification retries. | [session:217–243,364–384,423–463][session], [remote:757–790][remote] |
| D1 S | [],[empty],[empty,empty],[ab,c],[a,bc] distinct; empty item≠EOF; sending vs original epochs and receive/apply/copy/context sequences distinct. No codec yet. | [IDL:636–683][idl], [header:25–42][header], [ACK body:17–24][ack-body] |
| L1 S | Opened→Primary initial; Primary→Active demotes; Idle→Active retains secondary; Active→Idle rejected; None≠initial Opened. | [state:39–126][state] |
| L2 S | promotion transfers completed retained state only; demotion requires tail==commit/trims; changed provider progress7 resets next8/history, unchanged does not reset. | [queue-manager:58–160][qm], [loss:82–126][loss] |
| L3 S | direct child Close failure gives error/fault; outer host aborts/releases child and can complete Success, retaining diagnostic failure (not successful graceful child drain). | [direct Close:134–192][close], [outer Close:209–276][outer-close] |
| E1 S | tail12/commit10 → previous-epoch boundary12; duplicate primary epoch still calls provider; older epoch fails. | [manager:871–920][manager], [epoch:31–90][epoch] |
| E2 S | existing StartCopy(1,2): stale(1,1) drops, higher(1,3) faults receiver, not fresh streams. | [secondary:951–1003][secondary] |
| H1 S | stable secondary NotPrimary/NotPrimary; ready primary Granted/quorum-dependent; same-primary reconfig preserves read; demotion during/after catch-up Granted/TryAgain then TryAgain/TryAgain. | [status:34–145][status] |
| H2 S | promotion transition/catch-up TryAgain/TryAgain; catch-up completed Granted/TryAgain; completed Granted/quorum-dependent. | [status:58–71,101–114][status] |
| H3 D/S | schedule Temp1 then Temp2; explicit timer event emits Temp2 once; Close clears warning with OK, total2. Acceptance≠platform convergence. | [health test:107–135][health-test], [health:81–125][health] |

## Actual exposure and deterministic settings cases

These are source-derived **expected cases**, not implemented Kuberic settings.
Each maps to a concrete scenario above and remains pending port/run status.
All settings fields in the [base/EX1–EX4 public structs:2910–2960][types] are
included; internal policy is explicitly separate.

| Case → scenario | Fixed classification/default/update outcome | Evidence |
|---|---|---|
| settings2 → C1 | QI present; get returns merged effective settings/address/security | [QI][qi], [Com:801–808][com] |
| stream2 → C1 | QI present even with fault/EOS mode disabled | [stream QI:12–24][stream-qi] |
| specific-quorum → C1/M3 | public marker present; successor compares M≥Qcc | [QI][qi], [manager:1092–1099][manager] |
| atomic-group → C1 | absent from ComReplicator; separate ServiceGroup wrappers excluded | [QI][qi], [ServiceGroup:153–180][service-group] |
| internal batch/reservation → C1 | advertised internally; methods and batch stream E_NOTIMPL | [Com:1002–1034][com], [stream COM:48–64][com-stream] |
| internal queue counters → C1 | implemented internal query, not scalar/platform parity | [Com:1036–1073][com] |
| managed buffers → D1 | null Buffer or Count0→E_POINTER; constructor uses Buffer+Offset without an Offset bounds check here; delegate ordinary BeginReplicate | [Com:53–83,1094–1118][com] |
| V2 copy-latest → C1 | separate internal provider marker; not public specific-quorum or classic build target | [provider probe:19–26][provider], [V2 QI:10–29][v2] |
| stream fault → C1 | invalid kind→InvalidArgument before policy; disabled→InvalidOperation | [COM stream:67–78][com-stream], [stream:66–85][stream] |
| explicit EOS ACK → C2 | ordinary null EOF vs enabled explicit EOS requiring ACK | [get-operation:116–136][get-operation] |
| persisted state → S1 | true forces explicit service ACK even require=false | [secondary:165–193][secondary] |
| fault/load/move-cost/scoped health → H3 | reporting acceptance does not prove immediate platform convergence | [partition:208–350][partition] |
| retry → C6 | 5000ms; specified6000 public update rejected; unspecified/equal not this error | [defaults][defaults], [Com:490–532][com] |
| ACK batch → R3 | 15ms, strictly below retry5000ms | [defaults][defaults], [ACK][ack] |
| require ACK → S1 | false; volatile FF implicit, persisted TF explicit | [defaults][defaults], [secondary][secondary] |
| copy queue → C3 | initial64/max1024 count limits; no maximum-memory limit | [defaults][defaults], [copy:42–49][copy], [copy receiver:388–395][copy-receiver] |
| legacy InitialReplicationQueueSize → C6 | default64; specified128 sets both effective roles; later EX3 primary64 overrides primary only to64/128 | [defaults:136–139][defaults], [Com:550–582,639–762][com] |
| legacy MaxReplicationQueueSize → C6 | default1024; explicit1024 sets both effective counts; EX3 can override per role | [defaults:136–139][defaults], [Com:550–582,639–762][com] |
| legacy MaxReplicationQueueMemorySize → C6 | default0; specified600 without legacy maximum-count flag gives effective counts0/0; with max1024 gives1024/1024, before EX3 overrides and validation/comparison, not a successful live-upgrade claim | [Com:550–582,639–762][com] |
| primary queue → Q2 | initial64/max1024/memory0; retained bytes count | [defaults][defaults], [queue test][queue-test] |
| secondary queue → Q4 | initial64/max2048/memory0; history trims independently | [defaults][defaults], [queue test][queue-test] |
| message size → T2 | max52,428,800 bytes; too-large faults | [defaults][defaults], [session][session] |
| clear ACKed → Q4 | false public default; capability not latest progress | [defaults][defaults], [heads][qh] |
| EOS/fault mode → C6 | public false vs internal macro true | [public config:12–17][reconfig], [defaults:185–207][defaults] |
| pending quorum Close → C6 | public timeout0; no invented universal finite drain guarantee | [defaults][defaults] |
| addresses → C6 | localhost:0; listen/publish not compared as breaking fields | [defaults][defaults], [Com:514–515][com] |
| security → C6 | credential changes validated/updated, never ignored; not permission for arbitrary timer upgrades | [Com:762–785][com] |
| internal policy → M5/C5 | AllowMultipleQuorumSet=true; idle freshness60s, driven by explicit tick/pressure in future tests | [defaults:165–177][defaults] |
| max pending ACK → R3 | 32 default; config-store dynamic notifications≠arbitrary public upgrade | [defaults][defaults], [settings test:706–795][settings-test] |

## Current-engine counterexamples and existing witnesses

These preserve existing assertions. They do **not** turn current-engine behavior
into the native specification. Exact commands/results belong to the
[testing guide](testing.md#sf-v1-p1-foundation); source-only entries are not
executed observations.

| Native expectation | Current source / exact Rust witness | Evidence class |
|---|---|---|
| A1/E1 assignment and issued epoch separate from provider commit | `public_v1_recovery::read_fencing::sf_d02_primary_scalar_waits_for_quorum_and_local_commit_across_cancel_and_epoch`: peer ACK held→scalar0; quorum then local commit held→result/scalar pending0; after commit→1. Canceled/epoch case records provider tail0 vs report(0,1). [source:17–135][read-fencing] | actual existing engine witness; not native run |
| S1 persisted received progress before service ACK | `public_v1_recovery::read_fencing::sf_d02_secondary_scalar_is_contiguous_service_acknowledged_not_committed`: apply1/held service ACK→scalar0, after ACK→1; gap3 rejected. [source:138–168][read-fencing] | actual existing engine witness; not native run |
| L3 outer containment can succeed despite child Close failure | `public_v1_recovery::hosted_public_v1_contained_close_restarts_without_replaying_interrupted_children` expects outer error and records ordered Abort. [source:3523–3608][recovery] | actual existing engine witness; KSF-14 unchanged |
| Q3 retained capability | [current engine:2997–3008][current] returns committed tail, not retained floor; at committed100/no history native expects0 | source-reviewed numeric counterexample only |
| M2/M3 catch-up equations | [current engine:3031–3126][current] waits at local committed target with PC/CC/union checks | source-reviewed counterexample only |
| C2 target context | [current engine:1955–1964][current] calls source provider context | source-reviewed counterexample only |
| S2 shared/terminal streams | [current stream:315–365][current-stream] uses single-take getters; CopyComplete is not stream EOF | source-reviewed counterexample only |
| H1 secondary access | [current host:545–605][current-host] grants ready ActiveSecondary reads behind provider equality | source-reviewed counterexample only |

These sources are pinned to the P1 starting commit
`aa6c84224437d7e1ba6c5ad3d227f9426026f616`. Audit statuses stay unchanged:
7 fixed / 14 open plus SF-D01; SF-D02 historically fixed. Neither this ledger
nor its structural checks close findings or establish full optional-mode,
hosted, native differential or provider-fsync parity.

## Next boundary

P2 ports the queue/session/sender/ACK/async/tonic substrate **without production
activation**. P3 ports managers/receivers/copy/roles; P4 prepares service/host
adapters. Only P5 atomically switches factory, APIs/services/wire/reports/
controller and removes the old engine. See the
[roadmap](sf-v1-semantic-alignment-plan.md); P1 creates no permanent alternate
engine, endpoint or selector.

[native]: https://github.com/microsoft/service-fabric/tree/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d
[state]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicatorState.cpp#L39-L126
[qi]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComReplicator.h#L12-L35
[manager]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.cpp
[qm]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationQueueManager.cpp
[queue]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/OperationQueue.cpp
[qh]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/OperationQueue.h#L62-L87
[secondary]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicator.cpp
[receiver]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicationReceiver.cpp
[copy-receiver]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryCopyReceiver.cpp#L24-L80
[callback]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComFromBytesOperation.cpp#L167-L224
[idl]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/idl/public/FabricRuntime.idl#L636-L683
[sender]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReliableOperationSender.cpp
[remote]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/RemoteSession.cpp
[ack]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/AckSender.cpp
[copy]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/CopyAsyncOperation.cpp
[build]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.BuildIdleAsyncOperation.cpp#L111-L166
[copy-sender]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/CopySender.cpp#L524-L579
[transport]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationTransport.cpp#L880-L937
[primary]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.cpp#L632-L683
[header]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationOperationHeader.h#L25-L42
[epoch]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.UpdateEpochAsyncOperation.cpp#L31-L90
[loss]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.OnDatalossAsyncOperation.cpp#L82-L126
[status]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.ReadWriteStatusCalculator.cpp#L34-L145
[com]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComReplicator.cpp
[defaults]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/ServiceModel/reliability/replication/REConfigMacros.h
[partition]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/ComStatefulServicePartition.cpp#L208-L350
[drain]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/SecondaryReplicator.DrainQueueAsyncOperation.cpp#L56-L102
[health]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/BatchedHealthReporter.cpp#L62-L125
[api-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PublicApi.Test.cpp#L454-L504
[queue-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/OperationQueue.Test.cpp
[manager-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicaManager.Test.cpp#L154-L220
[catchup]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/PrimaryReplicator.CatchupAsyncOperation.cpp#L43-L104
[sender-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReliableOperationSender.Test.cpp#L102-L143
[stream]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/OperationStream.cpp#L66-L85
[enumerator]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComProxyAsyncEnumOperation.cpp
[copy-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/CopyAsyncOperation.Test.cpp#L418-L449
[reconfig]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/reconfig.h#L12-L17
[transport-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationTransport.Test.cpp#L594-L617
[session]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationSession.cpp
[ack-body]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicationAckMessageBody.h#L17-L24
[close]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/Replicator.CloseAsyncOperation.cpp#L134-L192
[outer-close]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Failover/ra/FailoverUnitProxy.CloseAsyncOperation.cpp#L209-L276
[health-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/BatchedHealthReporter.Test.cpp#L107-L135
[types]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/idl/public/FabricTypes.idl#L2910-L2960
[stream-qi]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComOperationStream.h#L12-L24
[service-group]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/ServiceGroup/Service/ServiceGroup.StatefulService.h#L153-L180
[com-stream]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComOperationStream.cpp#L48-L78
[provider]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ComProxyStateProvider.cpp#L19-L26
[v2]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/data/txnreplicator/ComStateProvider.h#L10-L29
[get-operation]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/OperationStream.GetOperationAsyncOperation.cpp#L116-L136
[settings-test]: https://github.com/microsoft/service-fabric/blob/3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d/src/prod/src/Reliability/Replication/ReplicatorSettings.Test.cpp#L706-L795
[read-fencing]: https://github.com/youyuanwu/kuberic/blob/aa6c84224437d7e1ba6c5ad3d227f9426026f616/kuberic-runtime/tests/support/read_fencing.rs
[recovery]: https://github.com/youyuanwu/kuberic/blob/aa6c84224437d7e1ba6c5ad3d227f9426026f616/kuberic-runtime/tests/public_v1_recovery.rs#L3523-L3608
[current]: https://github.com/youyuanwu/kuberic/blob/aa6c84224437d7e1ba6c5ad3d227f9426026f616/kuberic-runtime/src/replicator/stateless/mod.rs
[current-stream]: https://github.com/youyuanwu/kuberic/blob/aa6c84224437d7e1ba6c5ad3d227f9426026f616/kuberic-runtime/src/replicator/stream.rs#L315-L365
[current-host]: https://github.com/youyuanwu/kuberic/blob/aa6c84224437d7e1ba6c5ad3d227f9426026f616/kuberic-runtime/src/host/hosting.rs#L545-L605
