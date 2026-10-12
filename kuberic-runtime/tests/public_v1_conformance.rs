use std::path::PathBuf;

#[test]
fn public_v1_conformance_oracle_has_exactly_ksf_01_through_ksf_16() {
    kuberic_runtime::testing::assert_public_v1_conformance_subject("strict-custom");

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
