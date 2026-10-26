# PostgreSQL Metadata: Deferred Disposition Gate

**PROPOSED / NOT IMPLEMENTED. Metadata removal remains deferred.** Current
[PostgreSQL](design.md) retains its checksummed metadata and PGDATA with distinct
owners. The [single project roadmap](../kuberic/sf-v1-semantic-alignment-plan.md#hosting-redesign-roadmap--not-implemented)
owns generic host/agent/controller migration; this is a **separate removal gate**,
not another competing generic implementation plan.
Generic [M1 and bounded M2 integration](../kuberic/ordinary-hosting-m1.md) are
implemented. Independent PG ordinary Open, authenticated native sessions,
exact joint/current continuation, physical SQL/pre-auth fencing and actual
Close-after-Abort joins are qualified in routine smoke. This removes no durable
PG field. Neither that acceptance nor the old unsupported-controller wrappers
satisfy any field-removal row below; exhaustive M3 and final workflow reviews
remain separate.

## Superseded prerequisites

The [original prerequisite proposal at the research baseline][historical]
is preserved in Git history. Its cold-restoration, monotonic handoff and
target-first switchover concerns are questions to reassess against current
sources, not presumed current defects. Its mandatory fresh-invocation-floor
catch-up rule, generic lower-bound certificates and KSF checklist are **not
adopted** by the hosting redesign. They must not override
[current captured-boundary/All semantics](../kuberic/classic-v1-runtime.md#quorum-catch-up-and-progress).
Existing actual PG formation/terminal evidence is not complete controller
recovery or metadata-removal evidence.

The old claim that those three generic changes are necessary/sufficient
prerequisites is retired. Peer-dependent recovery after endpoint Open belongs
to the new host roadmap. Any later handoff or recovery change needs a
source-backed, claim-specific design and acceptance proof; no public protected
method-set change is implied.

## Required field-by-field proof before removal

Inventory **every** `PgDurableState` field, every nested build/recovery record,
and all production read/write sites at the then-current source revision.
Current entry points:
[metadata schema/identity:18–88](../../../examples/postgres/src/durable.rs#L18-L88),
[native policy:144–164](../../../examples/postgres/src/durable.rs#L144-L164),
[native Build/terminal owner](../../../examples/postgres/src/public_v1.rs),
[private session/continuation/retirement](../../../examples/postgres/src/public_session.rs).
These anchors identify present owners, not a completed inventory.

Each row must specify:

| Required dimension | Proof obligation |
|---|---|
| Present use and target owner | Exact field/read/write sites → PGDATA/live PG, existing generic authority, process memory, disposable scratch or deletion. No opaque generic application payload/cache. |
| Reconstruction | Process, Pod and full-cluster restart sources; what cannot be recovered and must remain closed. |
| Completion replacement | Which ordinary callback/generic achieved stage replaces a former durable cursor; no raw scalar or role flag promoted into stronger evidence. |
| Irreversible effects | Crash/cancellation before and after PG/filesystem/helper effects; actual process/task joining and sticky error ownership. |
| Identity/lineage | Epoch/configuration, source/target storage/process/creation and timeline/system-identifier checks; rejection after old detail is compacted. |
| Retention | Acknowledged-prefix and PC/CC/policy obligations; bounded retired-ID watermarks; no TTL-forgetting uncertainty or deletion of the last carrier. |
| Verification | Focused real-PG crash/process-boundary cases and independent ordinary-engine host/controller acceptance; explicit commands, revision, outcomes and limitations. |

Cover identity/path binding and process generations; copied native identity/
progress; role/access/stopped state; synchronous policy/generation/certified
progress; accepted authority; inbound/outbound/suspended Build and catch-up;
retired IDs/attempts/epochs; recovery election/responders/fences/promotion/follow
stages. Do not assume PGDATA alone reconstructs host-selected workflow history
or orphan ownership.

No code may stop reading/delete the store or metadata root until every row
has an approved, implemented and tested disposition. A missing row blocks
removal; it does not justify moving PG certificates, timeline transcripts,
promotion receipts or arbitrary JSON/bytes into AgentStore, CRD status or a
generic relay.

## Retained safety and qualification obligations

- Preserve acknowledged-prefix recovery across compatible timelines and every
  still-relevant PC/CC/policy quorum obligation; reject unsafe intersection,
  missing authoritative prefix or incompatible lineage.
- Keep access closed until native role/configuration/catch-up and physical SQL
  fences complete. Endpoint readiness, scalar progress and copy success are
  not write admission.
- Preserve exact process/helper/gateway owners and containment across
  interrupted backup, promotion, demotion and metadata commit; API timeout
  cannot certify orphan death.
- Keep marked-successor PG WriteQuorum unsupported until separately designed;
  existing unmarked/All and capability fallback are not replaced by the old
  fresh-floor prescription.
- Rehearse compatible progress above a required handoff boundary, peer-dependent
  cold restoration, promotion/attachment ordering and shorter/incompatible
  survivors **where a later source-backed design establishes applicability**.
  Historical blocker labels alone are not that evidence.

Use [current validation tiers/selectors](../kuberic/testing.md#hosting-redesign-baselines-and-future-acceptance)
as baselines; add claim-specific missing cases in later authorized work.
No Rust/PG suite was rerun for this documentation delivery, and no metadata-removal
implementation is supplied. Native differential, production upgrades and downstream engines remain
unqualified.

[historical]: https://github.com/youyuanwu/kuberic/blob/b37009f8af7a7b2ed91f58f4889ed74e06013861/docs/features/postgres/stateless-metadata-prerequisites.md
