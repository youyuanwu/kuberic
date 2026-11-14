#![cfg(target_os = "linux")]

use kuberic_kind_tests::host::Cluster;

#[test]
#[ignore = "requires an explicitly owned KinD cluster; run just kind-test"]
fn bootstrap_quorum_write_read_and_owned_teardown() {
    Cluster::from_env().unwrap().scenario(false).unwrap();
}

#[test]
#[ignore = "requires an explicitly owned KinD cluster; run just kind-test"]
fn deleting_during_bootstrap_never_publishes_clients_and_cleans_up() {
    Cluster::from_env().unwrap().scenario(true).unwrap();
}
