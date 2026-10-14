# SF Public V1 Stateless Replicator: deferred MVP work

- Work ID: sf-public-v1-phase6
- Core outcome: Prove a dormant store-free built-in stateless Replicator preview end to end while production remains on the legacy engine.

## Decisions

### P6D-01: Production activation and legacy-engine removal
- Status: DEFERRED
- Reason: Phase 6 validates a dormant preview. Removing the managed engine, closing KSF-16 and activating the replacement require the separately reviewed replacement-only artifact and atomic cutover boundary.
- MVP alternative: Keep protocol 9/schema 6 and the legacy engine as the ordinary production selection while the preview is explicitly test-selected.
- Revisit when: Every Phase 6 acceptance gate passes and Phase 7 production cutover is authorized.
- Evidence: `docs/features/kuberic/service-fabric-alignment.md` separates Phase 6 preview validation from Phase 7 replacement-only release and activation.

### P6D-02: Pre-cutover state migration and rolling compatibility
- Status: DEFERRED
- Reason: The approved Phase 6 journey starts from fresh preview state and does not need to preserve or enroll pre-cutover agent or application storage.
- MVP alternative: Use fresh preview stores, reject incompatible state and keep mixed-version participation unsupported.
- Revisit when: A production adoption requirement explicitly requires retained data, rolling upgrades or mixed-version operation.
- Evidence: `docs/features/kuberic/stateless-default-replicator.md` defines fresh-storage activation and lists rolling or mixed-version migration as a non-goal.

### P6D-03: Transparent completion of crash-interrupted client requests
- Status: DEFERRED
- Reason: Phase 6 requires provider durability before acknowledgement and conservative ambiguity after process loss, not a Replicator-owned write-recovery journal.
- MVP alternative: Return failure or disconnection for ambiguous completion and rely on application operation identities for safe retry.
- Revisit when: A separately reviewed client contract requires transparent continuation across host-process failure.
- Evidence: `docs/features/kuberic/stateless-default-replicator.md` excludes transparent crash continuation and requires conservative ambiguous-write handling.

### P6D-04: Public V1 transport authentication or peer authorization
- Status: DEFERRED
- Reason: Public V1 intentionally has no transport credential contract. Replica identity, session, epoch, history and build values are correctness fences rather than credentials.
- MVP alternative: Preserve exact lifecycle fencing without an authentication or authorization activation gate.
- Revisit when: A separate security requirement authorizes TLS identity, workload identity, tokens or another transport-credential design.
- Evidence: `docs/features/kuberic/replicator-boundary.md` and `docs/features/kuberic/stateless-default-replicator.md` document the unauthenticated design boundary.

### P6D-05: Optional V2 transactional durability
- Status: DEFERRED
- Reason: Phase 6 proves the SF V1-shaped public replacement using provider-owned application durability; a Replicator-owned transactional layer is not required for that outcome.
- MVP alternative: Keep state providers authoritative for application bytes, progress and retained history.
- Revisit when: A separately selected consumer requires the V2 transactional contract after the public-only V1 replacement is stable.
- Evidence: `docs/features/kuberic/service-fabric-alignment.md` places optional V2 after the public-only V1 replacement.

### P6D-06: Eagerly reap completed transport task handles
- Status: DEFERRED
- Reason: The dormant preview retains completed inbound RPC abort handles until Close or Abort. Existing lifecycle shutdown drains them, and Phase 6 tests have bounded lifetimes; changing active-task bookkeeping is not required for the accepted correctness outcome.
- MVP alternative: Retain complete Close/Abort drainage and bounded preview test lifetimes.
- Revisit when: Before Phase 7 production activation, or earlier if a sustained many-write soak demonstrates material process-memory growth.
- Evidence: Final review finding ASTRA-11 identified cumulative completed-handle retention as resource hygiene rather than a Phase 6 correctness blocker.
