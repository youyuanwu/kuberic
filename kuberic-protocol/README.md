# kuberic-protocol

Pure domain model for the level-triggered Kuberic stack.

The crate defines:

- replica identity, epoch, PC/CC, topology, provisioning, and transition types;
- explicit switchover requests, frozen handoffs, and terminal receipts;
- secondary scale-down intent, dual policies, preparation/quorum evidence,
  terminal local-retirement reports, and exact post-commit cleanup contracts;
- normalized Kubernetes and replica-agent observations;
- fenced protocol commands and reconciliation plans;
- validation for quorum, incarnation, epoch, and transition invariants;
- the deterministic, side-effect-free evaluator.

It intentionally has no Kubernetes, gRPC, async runtime, or filesystem
dependencies. Controllers and agents exchange these canonical types through
transport adapters such as `kuberic-wire`.

The evaluator covers write-closed bootstrap, same-cardinality replacement,
ordinary failover, planned switchover, secondary scale-down, and quorum loss.
Failover persists exact failure timing, fences routing before newer authority, requires PC and
outstanding-CC read quorum, selects from epoch-fenced deactivation/progress evidence, authorizes
only the elected safe prefix under the new fence, performs retained-history or
full-copy repair, and accepts only current-only quorum evidence. Quorum loss
publishes `NoWriteQuorum` without changing the data-loss epoch and restores
access when the same configuration quorum returns.

Configuration JSON omits duplicated `primaryId`, derives it from the unique
`Primary` member, and flattens each member's exact identity fields beside its
role. Legacy nested-member and explicit-primary JSON remains readable for
durable metadata compatibility.

## Integration boundary

The controller is the only owner of desired-cluster evaluation. Agents and
runtimes may validate canonical protocol values, but they do not select a new
configuration or infer authority from Kubernetes readiness, routing, or raw
application progress.

The supported evaluator contract is full-set bootstrap,
same-cardinality replacement, ordinary failover, planned switchover,
secondary-only scale-down, and non-destructive quorum loss/recovery. Scale-up,
primary removal, timed replica dropping, destructive data-loss recovery, and
mixed-version negotiation remain fail-closed.

Protocol 6 defines secondary scale-down, with pure evaluation available behind
`EvaluationConfig::enable_secondary_scale_down` (library default **false**,
enabled by the production controller). Its typed intent removes exactly the
highest logical-ID committed secondary, preserves
the exact primary and retained members, and validates previous/reduced majority
policies independently (including 2→1). Preparation freezes a durable
write-closed primary boundary; admission carries retained previous-read-quorum
evidence, while current-only completion and cleanup require reduced-write-quorum
evidence including the unchanged primary. The removed member supplies no reduced
credit. Cleanup freezes Pod, PVC, and endpoint names and UIDs, or explicit
authoritative exact-name absence, separately from accepted topology.

Lowering `spec.replicas` requests sequential single-secondary removal; the desired
count is target and minimum, down to one. Increasing accepted membership is unsupported.
The evaluator freezes intent before preparation, persists PC read evidence before
reduced PC/CC dispatch, freezes reduced write evidence before current-only
dispatch, and atomically accepts reduced topology/policy with a cleanup receipt.
Each command uses one freshly observed exact session; target availability never
changes selection or grants reduced quorum credit. Primary loss waits without
retargeting, failover, or rollback. A later desired count waits for cleanup.

Post-commit local receipt publication (`AcceptSecondaryRemovalCommit`), stable
write grant, routing, retirement, and resource cleanup are separate decisions.
The local publication command binds one retained identity and the immutable
commit evidence; it bridges the runtime's existing accepted-receipt gate rather
than treating current-only admission as permission to write. Commands and reports
are wired through the controller, wire adapter, and durable agent execution;
the controller performs exact-resource observation and deletion.

`SecondaryScaleDownResourceObservation` requires authoritative exact-name
lookups and a proven Pod/mounted-PVC mapping. `DeleteScaleDownResource` carries
frozen name/UID and fresh resource version. Endpoint cleanup
precedes exact Pod fencing; PVC cleanup requires authoritative Pod-UID absence.
Same-name replacement UIDs are never adopted or deleted, including after the
cleanup obligation is cleared. Unknown extra resources do not confer deletion authority.
The target never contributes reduced quorum credit; after commit, exact Pod
deletion and observed absence substitute for an unavailable local retirement
reply. Cleanup permanently deletes the frozen PVC and serializes new operations.
Existing status JSON defaults the optional fields to absent; absence never
supplies scale-down authority.

Installed configuration authority is not command completion: an exact pending
PC/CC, current-only, or retirement command is replayed with its immutable evidence.
New admissions precede installed-pending replays so catch-up cannot starve primary
admission. Conflicting pending work is neither overwritten nor credited.

Cleanup completion replaces the deletion obligation with one bounded
`lastSecondaryRemoval` receipt containing only the immutable admission and
current-only quorum proof (no retirement report). It authorizes retained-member
convergence while its exact reduced topology is accepted, never resource deletion
or retirement. Late members first finish
PC/CC, then current-only, with writes closed. Before another removal supersedes
this proof, each retained member must have completed current-only evidence in
the receipt or currently attest completed local acceptance; otherwise
`ScaleDownRetainedMemberPending` waits for the late member. The next accepted
removal replaces the receipt, not an accumulating history. The optional status
field preserves restart recovery without re-authorizing cleanup of replacements.
After a later failover or replacement, that receipt can also validate a returning
exact accepted member's older, write-closed current-only removal report. This is
bounded local history only. A retained secondary missing local commit acceptance
first receives the exact `AcceptSecondaryRemovalCommit` with `localRecovery`.
It must attest the frozen current-only authority and verified boundary; unrelated
pending work blocks recovery. Only a subsequent report proving local acceptance
permits normal stale-authority correction. This step preserves cluster status and
the receipt, requires no old live quorum, and
historical evidence grants no writes, quorum votes, cleanup, or new transition.
Endpoint scaffolding and accepted-authority convergence precede new transition
admission; switchover then precedes reduction, which precedes replacement of an
unavailable selected removal target.

Scale-down progress projects stable reasons for preparation, previous read quorum
(`ScaleDownPreviousReadQuorumUnavailable`), reduced write quorum and verified
catch-up (`ScaleDownReducedWriteQuorumUnavailable`, `ScaleDownReducedCatchUpPending`),
current-only quorum, exact primary recovery (`ScaleDownPrimaryUnavailable`),
retirement, exact Pod fencing/absence, and cleanup. `ScaleUpUnsupported` and
`SpecDriftUnsupported` leave the latest desired generation unsatisfied.

Switchover requires a nonempty request ID and a committed logical secondary
ID. Identical active or latest-receipted requests are idempotent; cancellation,
retargeting, and conflicting reuse of that receipt's ID are rejected through
conditions. Only one terminal receipt is retained, not an operation history.
Admission rejection uses `SwitchoverRejected`; the receipt enum's `rejected`
value is reserved and not currently emitted by the evaluator.

Planned switchover freezes an exact source, named target, membership, policy,
and handoff certificate. Definitive target loss before authority admission
retires preparation and restores service at the starting authority. After any
requested authority admission, recovery requires the source's whole retained
certificate and a read quorum, and allocates a strictly newer configuration
epoch without changing the data-loss number. Compensation installs write-closed
PC/CC and current-only authority on every surviving exact participant before
stable write grant and routing. An absent exact secondary is not rebound.
An extant permanently faulted non-primary is first fenced by exact Pod deletion
with its PVC preserved; absence must be re-observed before survivor convergence.
Accepted target outcomes always converge forward.

Restoration durably retires the accepted spec generation and exact preparation ID
even if preparation was never observed, without inventing a handoff boundary.
An authority-bound high-water mark rejects all older/equal retired preparations
across repeated same-authority restorations and restart; deterministic identity
also binds the starting configuration and generation.
The source retains one retired certificate for compensation after current-only
completion but before the cluster receipt is persisted.

Temporary observations wait with bounded requeues. An impossible operation
retains its frozen intent in safety convergence: routing is removed and every
extant possible writer must report closed access under accepted or superseding
authority, or its exact Pod must be observed absent. Safety deletion is Pod-only,
UID/resourceVersion-fenced, and preserves PVCs. The terminal unsafe receipt is
published only after that proof; it never starts ordinary failover or replacement.

See the [level-triggered operator guide](../docs/features/kuberic/level-triggered-operator.md)
and [secondary scale-down](../docs/features/kuberic/level-triggered-operator.md#secondary-scale-down)
for usage, target/minimum risks, and the protocol-6/schema-2 fresh-deployment contract.

Replacement admission durably records `pendingReplacementCleanup` **before**
creating replacement scaffolding or provisioning. Authoritative exact-name GETs
freeze the old Pod/PVC names and UIDs and the peer Service name/UID (or confirmed
absence). PVC generation provenance and the exact replaced identity bind this
receipt; provisioning and replacement/failover transition IDs also bind its
contents. Missing provenance or failed reads prevent admission.

The receipt survives provisioning retries and failover adopting the replacement.
Topology acceptance atomically moves it, unchanged, to `lastReplacement`; it
never rediscovers old resources from label-selected lists. The two optional
status fields are mutually exclusive phases of **one** obligation, not a queue
or overwriteable history slot. Existing JSON without the new field still
decodes; legacy in-flight replacement intent without frozen provenance waits
closed rather than inferring deletion authority at acceptance.

After acceptance, exact-name GETs classify the frozen UID, a different same-name
UID, NotFound, or failure. Different UIDs prove the old resource absent but never
become cleanup targets. Exact endpoint/Pod/PVC absence clears the receipt before
another operation (including replacement/provisioning)
can start. Finalizers, lost delete replies, and controller restarts retain the
receipt; same-name replacement resources never inherit its deletion authority.
