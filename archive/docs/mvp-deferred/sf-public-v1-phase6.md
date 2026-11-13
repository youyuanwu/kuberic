# SF Public V1 Stateless Replicator: deferred MVP work

- Work ID: sf-public-v1-phase6
- Core outcome: Prove a dormant store-free built-in stateless Replicator preview end to end while production remains on the legacy engine.

## Decisions

### P6D-01: Production activation and legacy-engine removal
- Status: IMPLEMENTED
- Reason: Phase 7 removed the legacy engine, selectors, private attachments, continuation stores, old protocol/controller branches, and fallback. Public V1 is now the sole built-in path.
- MVP alternative: None; the direct replacement is complete.
- Revisit when: Never for compatibility. Production activation remains a separate fresh-state operational decision.
- Evidence: [current selection](../features/kuberic/classic-v1-runtime.md#selection-and-ownership) and [ownership](../features/kuberic/stateless-default-replicator.md).

### P6D-02: Pre-cutover state migration and rolling compatibility
- Status: REJECTED
- Reason: The direct replacement intentionally supports only fresh schema-12/controller/provider state. Old and partial state rejects unchanged.
- MVP alternative: Fresh deployment with explicit prior-state rejection.
- Revisit when: Only through a separately approved product decision that changes the no-compatibility design.
- Evidence: Phase 7 immutable rejection tests and `docs/features/kuberic/stateless-default-replicator.md`.

### P6D-03: Transparent completion of crash-interrupted client requests
- Status: DEFERRED
- Reason: Phase 6 requires provider durability before acknowledgement and conservative ambiguity after process loss, not a Replicator-owned write-recovery journal.
- MVP alternative: Return failure or disconnection for ambiguous completion and rely on application operation identities for safe retry.
- Revisit when: A separately reviewed client contract requires transparent continuation across host-process failure.
- Evidence: `docs/features/kuberic/stateless-default-replicator.md` excludes transparent crash continuation and requires conservative ambiguous-write handling.

### P6D-04: Public V1 transport authentication or peer authorization
- Status: SUPERSEDED BY CURRENT BEARER AUTHENTICATION
- Decision context: This replacement-era deferral concerned a then-credential-free
  transport; it is not the current security boundary.
- Current disposition: Production checks the host agent bearer token over HTTP.
  Identity/history/build fences remain separate from credentials; TLS is absent.
  Native credential-settings/update capability is still not exposed.
- Revisit when: Deployment-security qualification or broader credential APIs are
  separately authorized; do not treat ordinary network/token protection as rare.
- Evidence: [current security and direct source citations](../features/kuberic/classic-v1-runtime.md#production-policies-and-security).

### P6D-05: Optional V2 transactional durability
- Status: DEFERRED
- Reason: Phase 6 proves the SF V1-shaped public replacement using provider-owned application durability; a Replicator-owned transactional layer is not required for that outcome.
- MVP alternative: Keep state providers authoritative for application bytes, progress and retained history.
- Revisit when: A separately selected consumer requires the V2 transactional contract after the public-only V1 replacement is stable.
- Evidence: [separate V2 design](../features/kuberic/v2-transactional-replicator-design.md) and [current V1 pending work](../features/kuberic/sf-v1-semantic-alignment-plan.md).

### P6D-06: Eagerly reap completed transport task handles
- Status: IMPLEMENTED
- Reason: Completion-aware inbound and outbound registries remove finished entries during ordinary operation while preserving authority cancellation and Close/Abort drainage.
- MVP alternative: None.
- Revisit when: A measured task-lifecycle regression appears.
- Evidence: Public V1 recovery tests run two 1,000-RPC batches and assert raw retained cardinality returns to baseline.
