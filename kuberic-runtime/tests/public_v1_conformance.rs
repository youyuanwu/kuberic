#![cfg(kuberic_workspace_tests)]

#[test]
fn public_v1_conformance_oracle_has_exactly_ksf_01_through_ksf_16() {
    kuberic_runtime::testing::assert_public_v1_conformance_subject("built-in-replacement");
    assert_eq!(
        kuberic_runtime::testing::public_v1_postgres_conformance_scenarios(),
        vec![
            "public_v1_postgres_build_cancellation_and_retirement",
            "public_v1_postgres_contained_close",
            "public_v1_postgres_data_loss_history_and_replay",
            "public_v1_postgres_primary_lifecycle_and_joint_configuration",
        ]
    );

    let inventory = kuberic_runtime::testing::replacement_ownership_inventory();
    kuberic_runtime::testing::validate_replacement_ownership_inventory(&inventory).unwrap();
    let mutation =
        kuberic_runtime::testing::replacement_ownership_inventory_with_extra_endpoint_fixture();
    assert_eq!(
        &mutation.services[..inventory.services.len()],
        inventory.services.as_slice(),
        "endpoint mutation changed the reviewed service manifest"
    );
    assert_eq!(
        mutation.services.last().map(String::as_str),
        Some("unmanifested runtime service registration")
    );
    assert!(
        kuberic_runtime::testing::validate_replacement_ownership_inventory(&mutation).is_err(),
        "KSF-16 canonical eligibility accepted an unmanifested reachable endpoint"
    );
    let mutation =
        kuberic_runtime::testing::replacement_ownership_inventory_with_alternate_factory_fixture();
    assert_eq!(
        &mutation.factories[..inventory.factories.len()],
        inventory.factories.as_slice(),
        "factory mutation changed the reviewed factory manifest"
    );
    assert_eq!(
        mutation.factories.last().map(String::as_str),
        Some("unmanifested built-in factory construction")
    );
    assert!(
        kuberic_runtime::testing::validate_replacement_ownership_inventory(&mutation).is_err(),
        "KSF-16 canonical eligibility accepted an unmanifested reachable factory"
    );
    let mutation =
        kuberic_runtime::testing::replacement_ownership_inventory_with_alternate_protocol_fixture();
    assert!(
        kuberic_runtime::testing::validate_replacement_ownership_inventory(&mutation).is_err(),
        "KSF-16 mutation oracle accepted an alternate protocol"
    );
    let mutation =
        kuberic_runtime::testing::replacement_ownership_inventory_with_alternate_controller_fixture(
        );
    assert_eq!(
        &mutation.controller_modules[..inventory.controller_modules.len()],
        inventory.controller_modules.as_slice(),
        "controller-helper mutation changed the reviewed controller manifest"
    );
    assert_eq!(
        mutation.controller_modules.last().map(String::as_str),
        Some("unmanifested controller startup helper")
    );
    assert!(
        kuberic_runtime::testing::validate_replacement_ownership_inventory(&mutation).is_err(),
        "KSF-16 canonical eligibility accepted an unmanifested reachable controller helper"
    );
}

#[test]
fn public_v1_conformance_partitions_cover_every_registered_observation_once() {
    kuberic_runtime::testing::assert_public_v1_conformance_partition_coverage();
}

#[test]
#[ignore = "bounded native/controller source execution partition"]
fn public_v1_conformance_executes_built_in_replacement_scenarios() {
    kuberic_runtime::testing::execute_public_v1_conformance_subject("built-in-replacement");
}

#[test]
#[ignore = "bounded source partition for the real service recreation journey"]
fn public_v1_conformance_executes_service_failover_after_durable_promotion() {
    kuberic_runtime::testing::execute_public_v1_conformance_partition(
        "built-in-replacement",
        "failover-after-durable-promotion",
    );
}

#[test]
#[ignore = "bounded source partition for the real service recreation journey"]
fn public_v1_conformance_executes_service_failover_at_persisted_promotion() {
    kuberic_runtime::testing::execute_public_v1_conformance_partition(
        "built-in-replacement",
        "failover-at-persisted-promotion",
    );
}

#[test]
#[ignore = "bounded source partition for the real service ambiguity/abort journey"]
fn public_v1_conformance_executes_service_ambiguous_commit_abort() {
    kuberic_runtime::testing::execute_public_v1_conformance_partition(
        "built-in-replacement",
        "ambiguous-commit-abort",
    );
}

#[test]
#[ignore = "bounded source partition for the real service handoff journey"]
fn public_v1_conformance_executes_service_switchover_after_catch_up() {
    kuberic_runtime::testing::execute_public_v1_conformance_partition(
        "built-in-replacement",
        "switchover-after-catch-up",
    );
}

#[test]
#[ignore = "bounded source partition for the real service handoff journey"]
fn public_v1_conformance_executes_service_switchover_after_durable_handoff() {
    kuberic_runtime::testing::execute_public_v1_conformance_partition(
        "built-in-replacement",
        "switchover-after-durable-handoff",
    );
}

#[test]
fn built_in_replacement_ksf_mappings_require_each_authorized_observation() {
    for (gate, scenarios) in [
        (
            "KSF-01",
            &[
                "replicator::v1::role::tests::initial_primary_queries_provider_before_epoch_and_requires_configuration",
                "host::service::ordinary_goal_tests::ordinary_goal_build_is_retained_exact_two_ended_and_remove_is_local_only",
                "retained_secondary_promotion_preserves_acknowledged_bytes_and_replaces_service_location",
                "m2_builtin_multi_secondary_joint_current_preserves_exact_sessions",
            ][..],
        ),
        (
            "KSF-02",
            &["helper_free_recovery_scopes_changed_unchanged_and_failed_observations"][..],
        ),
        (
            "KSF-03",
            &[
                "m2_builtin_crossed_identity_and_stale_ack",
                "public_v1_peer_withdrawal_advances_epoch_without_role_replay",
            ][..],
        ),
        (
            "KSF-04",
            &[
                "m2_builtin_target_context_and_ack_boundaries",
                "m2_builtin_multi_secondary_joint_current_preserves_exact_sessions",
                "replicator::v1::service_adapter::tests::facade::exact_binding_and_configuration_failures_do_not_start_providers_or_mutate_owners",
            ][..],
        ),
        (
            "KSF-05",
            &[
                "host::service::ordinary_goal_tests::ordinary_goal_build_is_retained_exact_two_ended_and_remove_is_local_only",
                "m2_builtin_target_context_and_ack_boundaries",
                "m2_builtin_lost_target_cancel_quarantines_receiver",
                "m2_builtin_crossed_identity_and_stale_ack",
                "public_build_consumes_target_context_and_installs_nonempty_snapshot",
            ][..],
        ),
        (
            "KSF-06",
            &[
                "m2_builtin_effect_fences_and_retained_provider_join",
                "m2_builtin_abort_close_retains_all_joins",
                "m2_application_sqlite_abort_close_retains_actual_sql_worker",
                "m2_application_scope_withdrawal_waits_for_actual_kv_consumer_close",
                "m2_builtin_exact_session_replay_and_retirement",
            ][..],
        ),
        (
            "KSF-07",
            &[
                "native_swap_freezes_specific_quorum_and_conservative_all_fallback_inputs",
                "host::public_lifecycle::tests::sf06_swap_retains_primary_through_catchup_then_pairs_actual_roles",
                "replicator::v1::replica_manager::tests::multiple_must_catchup_replicas_must_reach_advancing_current_quorum_not_saved_b",
                "replicator::v1::replica_manager::tests::all_wait_tracks_live_writes_and_completed_wait_does_not_reopen",
            ][..],
        ),
        (
            "KSF-08",
            &[
                "public_v1_history_partition_seed_is_shared",
                "native_query_outcome_matrix_survives_rpc_persistence_and_controller_eligibility",
                "native_query_not_queried_preserves_copy_detail_and_cannot_elect_or_build",
                "native_query_rejoin_matrix_distinguishes_source_capability_from_target_current",
                "public_v1_evidence_report_is_structurally_public_only",
                "helper_free_recovery_scopes_changed_unchanged_and_failed_observations",
                "sf01_primary_local_commit_hides_speculation_after_native_completion",
                "sf01_dropped_client_retains_service_owned_commit_and_observation",
                "sf11_acknowledged_prefix_6_through_10_survives_ambiguous_11_without_history_fabrication",
                "m2_builtin_crossed_identity_and_stale_ack",
            ][..],
        ),
        (
            "KSF-09",
            &["production_config_has_no_protocol_selector_or_application_commit_oracle"][..],
        ),
        (
            "KSF-10",
            &[
                "host::service::ordinary_goal_tests::ordinary_goal_builtin_factory_opens_actual_engine_listener_and_writer",
                "m2_builtin_abort_close_retains_all_joins",
                "m2_application_sqlite_close_joins_actual_consumers_and_retains_failure",
            ][..],
        ),
        (
            "KSF-11",
            &[
                "retained_secondary_promotion_preserves_acknowledged_bytes_and_replaces_service_location",
                "public_factory_primary_process_renewal_retains_exact_role_address_authority",
            ][..],
        ),
        (
            "KSF-12",
            &[
                "m2_application_sqlite_abort_close_retains_actual_sql_worker",
                "m2_builtin_lost_target_cancel_quarantines_receiver",
                "m2_builtin_abort_close_retains_all_joins",
            ][..],
        ),
        (
            "KSF-13",
            &[
                "native_transient_fault_restarts_same_storage_under_exact_session_renewal",
                "native_controller_unreachable_survivor_fences_until_exact_return_or_explicit_retirement",
                "primary_commit_failure_contains_the_ambiguous_native_quorum_success",
                "m2_builtin_lost_target_cancel_quarantines_receiver",
                "m2_application_copy_failures_preserve_retained_bytes_and_never_ack",
                "m2_application_kv_ordinary_copy_access_and_matching_format_reopen",
            ][..],
        ),
        (
            "KSF-14",
            &[
                "host::service::ordinary_goal_tests::ordinary_goal_builtin_factory_opens_actual_engine_listener_and_writer",
                "m2_builtin_abort_close_retains_all_joins",
                "host::service::ordinary_goal_tests::ordinary_goal_terminal_ids_reject_before_mutation_and_exact_retries_reopen",
            ][..],
        ),
        (
            "KSF-15",
            &[
                "in_process::journeys::sf01_primary_local_commit_isolation_and_abort_never_late_grants_success",
                "in_process::journeys::failover_recreation_at_persisted_promotion_preserves_effects",
                "in_process::journeys::failover_recreation_after_durable_promotion_consumes_completion_once",
                "in_process::journeys::switchover_recreation_after_catch_up_resumes_accepted_handoff",
                "in_process::journeys::switchover_recreation_after_durable_handoff_resumes_promotion",
                "host::service::ordinary_goal_tests::ordinary_goal_builtin_factory_opens_actual_engine_listener_and_writer",
                "m2_builtin_exact_session_replay_and_retirement",
                "host::service::ordinary_goal_tests::ordinary_goal_terminal_ids_reject_before_mutation_and_exact_retries_reopen",
                "public_v1_replacement_schema_failed_build_containment_survives_reopen",
                "host::service::ordinary_goal_tests::m2_ordinary_rebuild_error_preserves_exact_goal_wire_and_reopen",
                "public_v1_history_data_loss_attempt_renews_higher_epoch",
                "public_v1_data_loss_renewal_requires_exact_replay_capability",
                "public_v1_replacement_schema_build_ambiguity_requires_retirement",
            ][..],
        ),
        (
            "KSF-16",
            &[
                "m2_builtin_multi_secondary_joint_current_preserves_exact_sessions",
                "m2_builtin_crossed_identity_and_stale_ack",
                "m2_application_copy_failures_preserve_retained_bytes_and_never_ack",
                "m2_builtin_bound_ingress_rejects_body_epochs_before_native_mutation",
                "replicator::v1::transport::tests::malformed_presence_version_batch_and_full_route_are_rejected_not_delivered",
                "m2_builtin_exact_session_replay_and_retirement",
            ][..],
        ),
    ] {
        for scenario in scenarios {
            kuberic_runtime::testing::assert_public_v1_conformance_gate(
                "built-in-replacement",
                gate,
                scenario,
            );
        }
    }
}
