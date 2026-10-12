# SF Public V1 Simplification: deferred MVP work

- Work ID: sf-public-v1-simplification
- Core outcome: Decide whether the dormant Public V1 implementation needs behavior-preserving simplification before Phase 6.

## Decisions

### PVS-01: Consolidate controller session-renewal acceptance wrappers
- Status: PROPOSED
- Reason: Five thin wrappers exist, but they are named feature-gated test-bridge integration seams. Consolidation currently offers only a minor syntactic reduction without a demonstrated maintenance benefit.
- MVP alternative: Retain the explicit transitions.
- Revisit when: Phase 6 creates concrete call-site duplication or a measurable reduction beyond line count.
- Evidence: `kuberic-controller/src/evaluator/public_v1_session_renewal.rs` defines the acceptance wrappers, and `kuberic-controller/src/evaluator/test_bridge.rs` exposes them to cross-crate tests.

### PVS-02: Flatten PostgreSQL Public V1 dispatch into the legacy implementation
- Status: PROPOSED
- Reason: The branches preserve independent engines and exact protected callback dispatch. Flattening before cutover could change lifecycle and failure semantics rather than remove a behavior-free layer.
- MVP alternative: Keep the explicit protocol-selection branches.
- Revisit when: Phase 7 explicitly authorizes removal of the legacy engine or characterization proves callback-by-callback equivalence.
- Evidence: `examples/postgres/src/adapter.rs` contains the protected callback dispatch, while `examples/postgres/src/service.rs` owns the protocol-selection boundary.

### PVS-03: Merge Public V1 report authority derivation with callback-program selection
- Status: PROPOSED
- Reason: Both paths inspect the operation program, but one produces execution instructions and the other produces persisted completion evidence. A shared abstraction could move complexity without reducing it.
- MVP alternative: Preserve the exhaustive independent mappings and compiler coverage.
- Revisit when: Characterization proves one canonical descriptor can represent both mappings without weakening corrupt-state failures or wire evidence.
- Evidence: `kuberic-runtime/src/host/public_lifecycle_program.rs` selects execution instructions, while `kuberic-runtime/src/host/public_lifecycle.rs` derives completion evidence.

### PVS-04: Change schema-11 AgentState JSON layout or split Public V1 records into dedicated tables
- Status: PROPOSED
- Reason: The user confirmed that the schema-11 internal design is not fixed and compatibility is not required. A layout change is therefore allowed, but it still needs evidence that it reduces total implementation, verification, and operational complexity rather than merely moving it.
- MVP alternative: Keep the current layout until a measured simplification candidate is identified.
- Revisit when: Code research identifies a bounded layout change with a measurable reduction and explicit fresh-store or migration handling.
- Evidence: `kuberic-runtime/src/host/state.rs` defines the serialized state and `kuberic-runtime/src/host/sqlite_store.rs` enforces schema-11 storage boundaries.
- Decision update: On 2026-10-12, the user stated that the design is not fixed and compatibility is not required.

### PVS-05: Remove apparently test-only Public V1 controller or runtime surfaces
- Status: PROPOSED
- Reason: The surfaces are deliberate feature and configuration entry points consumed by cross-crate tests, generated protocol code, PostgreSQL, and the Phase 6 boundary. Dormancy is not evidence that they are unused.
- MVP alternative: Retain dormant feature gates and negative activation guards.
- Revisit when: Explicit feature-removal authorization or Phase 7 completion eliminates every configuration, generated, test, and external consumer.
- Evidence: `kuberic-runtime/src/testing/sqlite_store.rs` and `kuberic-controller/src/evaluator/test_bridge.rs` provide test consumers; `kuberic-runtime/build.rs` and `kuberic-controller/src/bin/crdgen.rs` provide generated-code consumers.
