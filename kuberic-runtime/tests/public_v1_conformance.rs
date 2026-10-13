use std::path::PathBuf;

#[test]
fn public_v1_conformance_oracle_has_exactly_ksf_01_through_ksf_16() {
    kuberic_runtime::testing::assert_public_v1_conformance_subject(
        "strict-custom+built-in-preview",
    );
    assert_eq!(
        kuberic_runtime::testing::public_v1_postgres_conformance_scenarios(),
        vec![
            "public_v1_postgres_build_cancellation_and_retirement",
            "public_v1_postgres_contained_close",
            "public_v1_postgres_data_loss_history_and_replay",
            "public_v1_postgres_primary_lifecycle_and_joint_configuration",
        ]
    );

    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let replicator = std::fs::read_to_string(manifest.join("src/replicator/mod.rs")).unwrap();
    let service = std::fs::read_to_string(manifest.join("src/host/service.rs")).unwrap();
    let authority = std::fs::read_to_string(manifest.join("src/authority.rs")).unwrap();
    let public_lifecycle =
        std::fs::read_to_string(manifest.join("src/host/public_lifecycle.rs")).unwrap();

    assert!(
        replicator.contains("struct ManagedReplicatorCapabilities"),
        "KSF-09 must reject the current managed-attached production artifact"
    );
    assert!(
        !public_lifecycle.contains("ManagedReplicatorCapabilities"),
        "KSF-09 Public V1 lifecycle must not consume managed capabilities"
    );
    assert!(
        service.contains("replication_listener = TcpListener::bind"),
        "KSF-10 must inventory the current agent-owned replication listener"
    );
    for owner in [
        "trait ReplicationProgressStore",
        "trait BuildAuthorityStore",
        "trait BuildProgressStore",
    ] {
        assert!(
            authority.contains(owner),
            "KSF-16 must reject the current legacy continuation owner: {owner}"
        );
    }
}

#[test]
#[ignore = "bounded nested conformance execution gate"]
fn public_v1_conformance_executes_strict_and_hosted_preview_scenarios() {
    kuberic_runtime::testing::execute_public_v1_conformance_subject(
        "strict-custom+built-in-preview",
    );
}

#[test]
fn built_in_preview_ksf_mappings_require_each_authorized_observation() {
    for (gate, scenarios) in [
        (
            "KSF-02",
            &["hosted_preview_authorized_data_loss_maps_only_the_effective_outcome"][..],
        ),
        (
            "KSF-06",
            &[
                "exact_build_is_cancelled_and_drained_before_removal_and_successor",
                "hosted_preview_build_dispatches_exact_attempt_and_waits_for_matching_completion",
                "hosted_preview_removal_drains_build_excludes_peer_and_rejects_stale_session",
            ][..],
        ),
        (
            "KSF-08",
            &[
                "public_v1_history_partition_seed_is_shared",
                "ambiguous_suffix_exactly_replays_quorum_peers_despite_higher_advertised_progress",
                "hosted_preview_switchover_dispatches_exact_intent_and_accepts_matching_completion",
            ][..],
        ),
        (
            "KSF-11",
            &["hosted_preview_role_address_publishes_replaces_and_clears"][..],
        ),
        (
            "KSF-12",
            &["hosted_preview_supersession_cancels_build_root_and_drains_descendant_before_close"]
                [..],
        ),
        (
            "KSF-13",
            &[
                "public_fault_reporting_closes_access_and_location_before_returning",
                "hosted_preview_unreachable_survivor_blocks_access_until_return_or_retirement",
                "hosted_preview_rebuild_retires_and_restarts_fresh_replacement",
                "hosted_preview_switchover_dispatches_exact_intent_and_accepts_matching_completion",
            ][..],
        ),
        (
            "KSF-14",
            &["hosted_preview_contained_close_restarts_without_replaying_interrupted_children"][..],
        ),
        (
            "KSF-15",
            &[
                "hosted_preview_contained_close_restarts_without_replaying_interrupted_children",
                "durable_build_application_never_reinvokes_the_build_callback",
                "public_operation_replay_matrix_converges_each_repeatable_program_family",
                "public_v1_history_admission_gates_all_data_loss_outcomes",
                "public_v1_replay_preserves_declared_disposition",
                "public_v1_schema11_build_ambiguity_requires_retirement",
            ][..],
        ),
    ] {
        for scenario in scenarios {
            kuberic_runtime::testing::assert_public_v1_conformance_gate(
                "built-in-preview",
                gate,
                scenario,
            );
        }
    }
}
