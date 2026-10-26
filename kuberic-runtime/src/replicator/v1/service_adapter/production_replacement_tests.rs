use super::super::engine_session::replacement_tests::fixture;
use super::*;

#[test]
fn m3_production_metadata_index_keeps_old_pc_and_fresh_cc_exact_native_incarnations() {
    let binding = fixture();
    let predecessor = binding.replacement().unwrap();
    let old_information = replica_information(&predecessor);
    let fresh_information = ReplicaInformation {
        identity: binding.secondary().identity,
        process_session_id: binding.secondary().process_session_id,
        storage_incarnation: Some(binding.secondary().storage),
        replication_address: binding.secondary().address,
        role: ReplicaRole::IdleSecondary,
        current_progress: crate::application::INVALID_LSN,
        catch_up_capability: crate::application::INVALID_LSN,
        must_catchup: false,
    };
    let mut old_binding = binding.clone();
    old_binding.target.scope.storage = predecessor.storage_incarnation.clone();
    old_binding.target.scope.process_session_id = predecessor.process_session_id.clone();
    old_binding.target.scope.creation_id =
        crate::protocol::types::ReplicatorCreationId::new("old-creation");
    old_binding.target.replication_address = predecessor.replication_address.clone();
    let make_peer = |information, native_route, incarnation| PeerBinding {
        conservative_progress: false,
        fresh_checkpoint: None,
        information,
        build: BuildConfig {
            route: native_route,
            remote_incarnation: incarnation,
            copy: copy_config(),
            context: None,
        },
        callbacks: BuildCallbacks {
            fence: Some(Fence::denied()),
            replication: Arc::new(|_| false),
            copy: Arc::new(|_| false),
            context_ack: Arc::new(|_, _| false),
        },
    };
    let mut peers = PeerBindings::new();
    retain_peer(
        &mut peers,
        make_peer(
            old_information.clone(),
            route(&old_binding, &old_binding.primary()).unwrap(),
            7,
        ),
    );
    retain_peer(
        &mut peers,
        make_peer(
            fresh_information.clone(),
            route(&binding, &binding.primary()).unwrap(),
            9,
        ),
    );
    assert_eq!(peers[&old_information.identity.replica_id].len(), 2);
    let old = peer_binding(&peers, &old_information).unwrap();
    let fresh = peer_binding(&peers, &fresh_information).unwrap();
    assert_eq!(old.build.remote_incarnation, 7);
    assert_eq!(fresh.build.remote_incarnation, 9);
    assert_ne!(old.build.route.destination, fresh.build.route.destination);
    assert_ne!(
        old.build.route.remote_address,
        fresh.build.route.remote_address
    );
    for mutation in 0..3 {
        let mut crossed = fresh_information.clone();
        match mutation {
            0 => crossed.process_session_id = old_information.process_session_id.clone(),
            1 => crossed.storage_incarnation = old_information.storage_incarnation.clone(),
            _ => crossed.replication_address = old_information.replication_address.clone(),
        }
        assert!(peer_binding(&peers, &crossed).is_none());
    }
}
