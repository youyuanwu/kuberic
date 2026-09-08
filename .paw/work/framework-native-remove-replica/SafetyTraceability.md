# Phase 4 Remove-Replica Safety Traceability

Deletion gate status: **passed and revalidated through Phase 6 on 2026-09-08**. Every SC-002 invariant maps to
retained framework-native, shared runner/kernel, Kubernetes provider, or
live-cluster coverage. No row relies only on the legacy remove pilot.

| # | SC-002 invariant | Retained passing replacement test(s) | Coverage |
|---:|---|---|---|
| 1 | Immutable operation mode | `framework_native_remove_replica_force_mode_is_immutable_and_replays_without_target`; `scale_down_freeze_rejects_target_generation_drift_after_preadmission` | Native workflow/domain |
| 2 | Pre-commit restart | `framework_native_remove_replica_restarts_at_every_durable_boundary_without_duplicate_effects` | Native workflow/kernel |
| 3 | Post-commit restart | `framework_native_remove_replica_restarts_at_every_durable_boundary_without_duplicate_effects`; `framework_native_remove_replica_route_preserves_terminal_before_status_ordering` | Native workflow/reconciler |
| 4 | Exact prepared command | `remove_replica_execution_reuses_the_exact_prepared_command`; `framework_native_remove_replica_fr019_exact_effect_dispatch_and_quarantine` | Native workflow/adapter |
| 5 | Direct-dispatch authority | `remove_replica_execution_boundaries_are_tagged_and_never_store_mutable_operation_state`; `framework_native_remove_replica_fr019_exact_effect_dispatch_and_quarantine` | Native workflow/adapter |
| 6 | One-use dispatch authority | `freshly_accepted_terminal_reloads_before_publication_and_permit_is_one_use` | Shared runner |
| 7 | Lost effect reply | `framework_native_remove_replica_restarts_at_every_durable_boundary_without_duplicate_effects`; `framework_native_remove_replica_fr019_exact_effect_dispatch_and_quarantine` | Native workflow/adapter |
| 8 | Uncertain persistence write | `conflict_unknown_write_and_persistence_failure_stop_before_redelivery` | Shared runner |
| 9 | Conflict reload | `conflict_unknown_write_and_persistence_failure_stop_before_redelivery`; `host_rejects_unsupported_format_and_reloads_after_unit_conflict` | Shared runner/provider |
| 10 | Authoritative observation recovery | `framework_native_remove_replica_restarts_at_every_durable_boundary_without_duplicate_effects`; `framework_native_remove_replica_fr019_observation_collection` | Native workflow/adapter |
| 11 | Exact primary-status gap without churn | `framework_native_remove_replica_route_primary_gap_has_no_status_churn` | Native reconciler |
| 12 | Exact target-status gap without churn | `framework_native_remove_replica_route_target_gap_has_no_status_churn` | Native reconciler |
| 13 | Commit evidence | `remove_replica_execution_transition_validation_is_monotonic`; `framework_native_remove_replica_fr019_terminal_validation` | Native workflow/adapter |
| 14 | Configuration and Force authority | `framework_native_remove_replica_force_mode_is_immutable_and_replays_without_target`; `framework_native_remove_replica_force_requires_primary_target_and_topology_fences` | Native workflow/domain |
| 15 | Correlated primary role evidence | `remove_replica_execution_captures_exact_retained_terminal_evidence`; `framework_native_remove_replica_fr019_observation_collection` | Native workflow/adapter |
| 16 | Correlated lifecycle evidence | `remove_replica_execution_captures_exact_retained_terminal_evidence`; `framework_native_remove_replica_fr019_observation_collection` | Native workflow/adapter |
| 17 | UID-fenced label cleanup | `framework_native_remove_replica_uid_fenced_label_and_delete_commands` | Native adapter |
| 18 | UID-fenced deletion | `framework_native_remove_replica_uid_fenced_label_and_delete_commands` | Native adapter |
| 19 | Post-commit connection cleanup | `missing_exact_primary_status_never_proves_connection_absence`; `framework_native_remove_replica_fr019_terminal_validation` | Retained domain/native adapter |
| 20 | Incarnation fencing | `framework_native_remove_replica_force_requires_primary_target_and_topology_fences`; `remove_replica_execution_reuses_the_exact_prepared_command` | Native workflow/domain |
| 21 | Epoch and generation fencing | `framework_native_remove_replica_force_requires_primary_target_and_topology_fences`; `remove_replica_execution_reuses_the_exact_prepared_command` | Native workflow/domain |
| 22 | Bounded redrive of at most three attempts | `exhausted_known_catch_up_state_is_failed_precommit_incomplete`; `remove_replica_execution_maximum_fault_history_and_terminal_fit_independent_bounds` | Retained domain/native admission |
| 23 | Corrupt record and malformed agent-status handling | `remove_replica_execution_distinguishes_incompatible_and_malformed_contracts`; `framework_native_remove_replica_route_isolates_malformed_agent_status`; `load_distinguishes_absence_success_and_malformed_objects` | Native workflow/reconciler/Kubernetes provider |
| 24 | Incompatible record handling | `remove_replica_execution_distinguishes_incompatible_and_malformed_contracts`; `checkpoint_dispositions_cover_rejected_and_incompatible` | Native workflow/shared runner |
| 25 | Distinct unsafe terminal handling | `remove_replica_execution_nested_terminal_denies_unknown_fields`; `restart_and_incomplete_states_use_distinct_typed_dispositions` | Native workflow/retained domain |
| 26 | Distinct inexact terminal handling | `remove_replica_execution_validates_completed_terminal_against_immutable_admission`; `framework_native_remove_replica_fr019_terminal_validation` | Native workflow/adapter |
| 27 | Terminal-before-status ordering and publication conflict | `framework_native_remove_replica_route_preserves_terminal_before_status_ordering`; `freshly_accepted_terminal_reloads_before_publication_and_permit_is_one_use` | Native reconciler/shared runner |
| 28 | Redelivery without duplicated uncertain effects | `framework_native_remove_replica_restarts_at_every_durable_boundary_without_duplicate_effects`; `framework_native_remove_replica_fr019_exact_effect_dispatch_and_quarantine`; `conflict_unknown_write_and_persistence_failure_stop_before_redelivery` | Native workflow/adapter/shared runner |
| 29 | Three-member admission | `remove_replica_execution_admission_is_structured_compact_and_derives_reduced_topology` | Native admission |
| 30 | Active admission | `remove_replica_execution_rejects_all_six_one_byte_over_bounds`; `remove_replica_execution_maximum_fault_history_and_terminal_fit_independent_bounds` | Native admission/kernel |
| 31 | Terminal admission | `remove_replica_execution_rejects_all_six_one_byte_over_bounds`; `remove_replica_execution_maximum_fault_history_and_terminal_fit_independent_bounds` | Native admission/kernel |
| 32 | Persisted-record owner identity | `framework_native_remove_replica_checkpoint_owner_is_exact_and_non_controlling`; `owner_reference_is_validated_locally_and_preserved_across_writes` | Native construction/Kubernetes provider |
| 33 | Owner garbage collection | `validates_real_api_cas_watch_compaction_and_ambiguous_recovery` (`KUBERNETES_CHECKPOINT_LIFECYCLE owner_gc=passed`) | Isolated live Kubernetes API |
| 34 | Retained-record cleanup authorization | `checkpoint_rbac_examples_are_structural_and_lifecycle_specific` | Shared RBAC authorization |

## FR-017 Outcome Matrix

The matrix is backed by executable outcomes rather than variant-name
enumeration:

- native checkpoint classification:
  `framework_native_remove_replica_fr017_classifies_actual_checkpoint_failures`;
- active/deadline and prepared-observation outcomes:
  `adapter_wait_and_deadline_requeue_are_clamped`,
  `observe_and_wait_persists_without_same_cycle_progression`, and
  `prepared_activity_rejection_is_an_active_wait`;
- terminal publication:
  `freshly_accepted_terminal_reloads_before_publication_and_permit_is_one_use`
  and `framework_native_remove_replica_fr019_publication_handoff`;
- rejected/incompatible/isolated:
  `checkpoint_dispositions_cover_rejected_and_incompatible`,
  `observation_mismatch_is_isolated`, and
  `framework_native_remove_replica_route_isolates_malformed_agent_status`;
- conflict reload, unknown-write reload, and persistence failure:
  `conflict_unknown_write_and_persistence_failure_stop_before_redelivery`;
- nondeterminism and bounded fuel:
  `nondeterminism_and_bounded_fuel_have_distinct_outcomes`.

All nine outcomes are exercised; none is declared contract-impossible.
Switchover retains its operation-specific matrix in
`test_durable_execution_switchover_pilot_fr017_operation_outcome_matrix`.

## FR-019 Responsibility Tests

The six independently named native remove tests are:

1. `framework_native_remove_replica_fr019_observation_collection`
2. `framework_native_remove_replica_fr019_authority_and_preparation`
3. `framework_native_remove_replica_fr019_exact_effect_dispatch_and_quarantine`
4. `framework_native_remove_replica_fr019_deadline_policy`
5. `framework_native_remove_replica_fr019_terminal_validation`
6. `framework_native_remove_replica_fr019_publication_handoff`

## Phase 4 Verification Record

- Native workflow/adapter/routing matrix: **18 passed**. This includes the
  three new Force-authority and every-boundary restart tests.
- Private native routing matrix: **6 passed**, covering terminal reload and
  publication, publication conflict/restart, incompatible contracts, exact
  primary/target status gaps without churn, and malformed agent isolation.
  Fine-grained redelivery and fencing remain covered by the native
  workflow/adapter and shared-runner tests named above.
- Native Force coverage uses a genuine `DurableRemoveMode::Force` admission
  with no target generation or live target. It proves replay keeps Force
  immutable, while exact primary incarnation/role/epoch/generation/control,
  any observed target incarnation, and retained topology remain fenced.
- Native restart coverage persists and reloads every scheduled, exposed,
  observed, and terminal boundary across the representative five-boundary
  path. Repeated exposed-boundary restarts remain quarantined with byte-exact
  commands, and the replica, label, and delete effects each retain exactly one
  dispatch identity across pre-commit and post-commit recovery.
- Phase 5 graduated the verified native route into the default production
  runtime. The test-only routing gate and legacy feature were removed after
  this matrix passed.
- Full operator all-features suite: **301 passed**.
- Remove-feature operator suite: **248 passed**.
- Kubernetes checkpoint provider matrix: **13 passed**.
- Retained cleanup RBAC authorization: **1 passed**.
- Explicit remove regressions retained before deletion: **2 + 1 + 2 passed**.
- Switchover pilot/unit regression matrix: **47 passed**.
- Live native test target: compiled; intentionally ignored until Phase 5
  production routing removes the legacy selector.
- Real owner-GC deletion gate:
  `KUBECONFIG="$HOME/.kube/kuberic-kind-config" cargo test -p kuberic-durable-execution --features kubernetes --test kubernetes_checkpoint_real validates_real_api_cas_watch_compaction_and_ambiguous_recovery -- --nocapture`
  — **passed**, including `KUBERNETES_CHECKPOINT_LIFECYCLE owner_gc=passed`.

## Phase 6 Final Results

- Canonical no-fault measurement:
  `cargo test -p kvstore --test reconciler test_framework_native_remove_replica_three_no_fault_measurement_samples -- --nocapture`
  — **passed, 3 samples**. Every sample recorded exactly 3 external effects,
  2 passive observations, 5 durable boundaries, and 6 accepted writes.

  | Sample | Active minimum | Active maximum | Terminal record | Terminal payload |
  |---:|---:|---:|---:|---:|
  | 1 | 3,373 bytes | 18,685 bytes | 4,245 bytes | 683 bytes |
  | 2 | 3,373 bytes | 18,685 bytes | 4,245 bytes | 683 bytes |
  | 3 | 3,381 bytes | 18,693 bytes | 4,245 bytes | 683 bytes |

- Exact one-byte-over matrix:
  `cargo test -p kuberic-operator remove_replica_execution_rejects_all_six_one_byte_over_bounds`
  — **passed** for 17 records, 4,097-byte input, 2,049-byte result,
  262,145-byte active record, 12,289-byte terminal record, and 4,097-byte
  terminal payload.
- Theoretical maximum-fault projection:
  `cargo test -p kuberic-operator remove_replica_execution_maximum_fault_history_and_terminal_fit_independent_bounds -- --nocapture`
  — **passed** with 16 records, 4,096-byte input, 2,048-byte result,
  182,589-byte projected active record, 11,453-byte projected terminal record,
  and a 4,096-byte terminal payload ceiling.
- Isolated-Kind validation exposed a real 4,096-byte prepared-command
  regression with production-length Kubernetes identities. The native contract
  now stores the already-encoded protobuf action as binary `ExactBytes` rather
  than hex text, and the serialized contract version advanced from 2 to 3.
  The representative prepared command decreased from 3,064 to 2,070 bytes in
  the unit fixture, and the live native removal subsequently passed.
- Default native/shared gates passed:
  `cargo test -p kuberic-operator framework_native_remove_replica`,
  `cargo test -p kuberic-operator durable_runner_tests`,
  `cargo test -p kuberic-operator checkpoint_store`, and
  `cargo test -p kvstore --test reconciler test_framework_native_remove_replica_`.
- Local repository gates passed:
  `cargo check --all-targets`, `cargo fmt --all -- --check`,
  `cargo clippy --all-targets -- -D warnings`, `cargo build --all-targets`,
  `cargo test -p kuberic-core -p kuberic-operator -p kvstore -p sqlite-replicated`,
  `cargo test --doc --workspace`, and the complexity script/unit tests.
- Full all-feature validation passed with the dedicated kubeconfig and
  repository-local PostgreSQL 16 tooling:
  `KUBECONFIG="$HOME/.kube/kuberic-kind-config" cargo test --all --all-features`.
  The live-test crate reported **5 passed, 1 ignored**; the reconciler target
  reported **81 passed**; PostgreSQL instance tests reported **3 passed**.
- Dedicated Kind validation passed without enumerating or touching CAPI
  clusters: isolated cluster probe, `just images`, the real provider test,
  `test_kvstore_k8s_status_healthy`, `test_kvstore_k8s_write_read`, and
  `test_kvstore_k8s_framework_native_remove_replica`.
- Real provider measurements remained:
  active checkpoint 673 bytes, terminal checkpoint 521 bytes, 2 accepted
  writes, 1 watch event / 1,163 canonical typed-event bytes, and
  `owner_gc=passed`.
- Final live native removal evidence:
  owner UID `901e4eb3-3df6-431f-8afb-e3e67c274dcf`, checkpoint
  `kuberic-checkpoint-c8b119b0d9a560b84ba18361a5334b9d`, terminal checkpoint
  4,769 bytes, removed pod `kvstore-2`, and 2 surviving pods with unchanged
  admitted UIDs. The test restored the shared live fixture to three healthy
  replicas afterward.
- All 34 SC-002 rows above retain their named replacement tests. No mapped
  test was removed during production graduation or Phase 6 measurement work.
