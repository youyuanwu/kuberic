# SF V1: Pending Alignment and Qualification

This is a current work queue, not a delivery-phase checklist. Host-alignment
baseline: Kuberic `69b1cfe3c85d296463da0def3e80eaca0ddcd0a7`; native SF
`3988b4518236d2d0a37f4d8bdcdf0a2093c5a61d`.
Read [current semantics](classic-v1-runtime.md) first and use the
[disposition ledger](sf-v1-port-ledger.md) to interpret historical SF IDs.
Completing P6 and replacing an old engine do not prove that no core defect remains.

Priorities below order **future separately authorized work**. “Verified difference” means
source-confirmed divergence, not a dynamically reproduced data-loss defect;
“unverified” means missing claim-specific qualification, not demonstrated failure.

## Selected-host Close and access disposition

### Outer Close outcome

The selected journal executor now succeeds after contained child failure,
retains the child diagnostic and fails on cleanup failure
([program:550–638][aligned-close]). Actual public-host failure/interruption/
retained-provider witnesses cover the bounded behavior
([public:3114–3505][aligned-close-tests]). Unrecorded destructive callbacks are
contained without reinvocation; recorded Cleanup is not a process-local join.
This closes the selected outer-outcome difference, not every native inner
callback sequence or generic application-task join.

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

## P2 — Focused qualification, not presumed defects

| Obligation | Current evidence / limit | Bounded next work and completion criterion |
|---|---|---|
| **SF-12 rejected-peer event ordering** | Healthy quorum versus hung minority exists ([public:2302–2433][minority]); marked-successor arithmetic exists ([manager tests:690–719][marked]). Neither is both controlled orders of a dispensable remote rejection versus required healthy response. | First establish native fault scope; exercise actual selected owners/public host with both orders. Required healthy evidence must yield the documented outcome regardless of dispensable response ordering, while indispensable-peer/local faults remain distinct. Do not restore the old proof loop or mark Fixed from engine removal. |
| **SF-11 historical trace shape** | Current prefix regression preserves acknowledged prefix before ambiguous suffix ([test:146–214][prefix]); it is not the removed-engine `(11,10)` reconciliation against peer5. | Define a narrowly justified selected-path retained-source/behind-peer case before adding a test. Record what it proves about acknowledged-prefix recovery and what remains outside it; do not claim historical negative controls ran now. |
| **Public copy context** | Distinct nonempty context is actual-component evidence ([test:1013–1091][context]); external public provider uses empty context ([fixture:196–207][empty-context]). | If public nonempty-context qualification is needed, use a helper-free provider with distinguishable target/source bytes including an empty item. Source must consume target data without supplying its own; keep component/public evidence separate. |
| **Mode/fault/interleaving coverage** | Persisted/required-ACK combinations are core-tested ([tests:121–172][modes]); selected production has fixed mode/policy choices ([production:121–158][policy]). | Build a claim-specific component/public matrix and close consequential holes only. Private required-ACK volatile mode is not an exposed public setting; do not expand P7 simply to fill coverage cells. |
| **PG terminal observations** | Orderly stopped progress and retained Abort cleanup have distinct witnesses ([tests:163–249][pg-progress], [940–1050][pg-abort]). | Separately scope any need for SQL racing shutdown, Abort progress snapshots or survivor ranking. Completion requires an actual bounded scenario; existing fsynced files and process cleanup do not establish these claims. |

### P2 — Source-contract wording

Protected Rustdoc still describes catch-up in broader committed/per-configuration
terms ([traits:107–119][catchup-doc]) and epoch callback as “exact durable tail”
([provider trait:53–55][epoch-doc]). Actual selected manager uses captured
`B`/current `Q`/live marked `M` catch-up and issued-tail epoch boundary
([manager:975–1024][catchup], [292–302][epoch]).

**Next work/criterion:** a separately reviewed source-comment-only correction
must agree with pinned native source, preserving every protected method/type
signature. This docs-only workflow does not edit Rust source or infer an unsafe
election defect from stale wording.

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
[catchup-doc]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/replicator/traits.rs#L107-L119
[epoch-doc]: https://github.com/youyuanwu/kuberic/blob/e2a47b6c631c974aa0135e81c3a7e3a439980ded/kuberic-runtime/src/traits.rs#L53-L55
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
