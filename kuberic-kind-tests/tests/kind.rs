#![cfg(target_os = "linux")]

use kuberic_kind_tests::{host::Cluster, Scenario};

#[test]
#[ignore = "requires an explicitly owned KinD cluster; run just kind-test"]
fn planned_switchover_preserves_data_fences_old_writer_and_updates_deployed_routing() {
    Cluster::from_env()
        .unwrap()
        .scenario(Scenario::Switchover)
        .unwrap();
}

#[test]
#[ignore = "requires an explicitly owned KinD cluster; run just kind-test"]
fn bootstrap_quorum_write_read_and_owned_teardown() {
    Cluster::from_env()
        .unwrap()
        .scenario(Scenario::Bootstrap)
        .unwrap();
}

#[test]
#[ignore = "requires an explicitly owned KinD cluster; run just kind-test"]
fn deleting_during_bootstrap_never_publishes_clients_and_cleans_up() {
    Cluster::from_env()
        .unwrap()
        .scenario(Scenario::DeleteDuringBootstrap)
        .unwrap();
}

#[test]
#[ignore = "requires an explicitly owned KinD cluster; run just kind-test"]
fn original_node_exits_before_remaining_pods_receive_deletion() {
    Cluster::from_env()
        .unwrap()
        .scenario(Scenario::StaggeredShutdown)
        .unwrap();
}

#[test]
#[ignore = "requires an explicitly owned KinD cluster; run just kind-test"]
fn primary_application_crash_preserves_data_and_updates_deployed_operator_routing() {
    Cluster::from_env()
        .unwrap()
        .scenario(Scenario::PrimaryAppFailover)
        .unwrap();
}

#[test]
#[ignore = "requires an explicitly owned KinD cluster; run just kind-test"]
fn secondary_application_crash_restores_redundancy_without_changing_deployed_primary() {
    Cluster::from_env()
        .unwrap()
        .scenario(Scenario::SecondaryAppRepair)
        .unwrap();
}

#[test]
#[ignore = "requires an explicitly owned KinD cluster; run just kind-test"]
fn copy_held_fresh_replica_cannot_restore_serving_or_voting_before_native_completion() {
    Cluster::from_env()
        .unwrap()
        .scenario(Scenario::CopyHeldSecondaryRepair)
        .unwrap();
}
