use super::*;
use crate::protocol::public_operations::PublicReplicaDescriptor;

pub(super) async fn installed_primary() -> (Fixture, PublicConfiguration) {
    let fixture = Fixture::new().await;
    let local = fixture
        .store
        .load_state()
        .await
        .unwrap()
        .identity
        .local_identity;
    let role = fixture
        .launch(
            PublicOperationProgram::Role {
                epoch: Epoch::new(0, 3),
                role: ReplicaRole::Primary,
            },
            1,
        )
        .await;
    assert_eq!(
        completed(&role).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    let configuration = PublicConfiguration {
        current: ConfigurationDescriptor::new(
            Epoch::new(0, 3),
            local.replica_id,
            vec![ConfigurationMember {
                identity: local,
                role: ReplicaRole::Primary,
            }],
            1,
        ),
        previous: None,
        replicas: Vec::new(),
    };
    let installed = fixture
        .launch(
            PublicOperationProgram::Configuration(configuration.clone()),
            2,
        )
        .await;
    assert_eq!(
        completed(&installed).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    (fixture, configuration)
}

fn swap(starting: PublicConfiguration, controller_composed: bool) -> PublicOperationProgram {
    let mut refreshed = starting.clone();
    refreshed.current = ConfigurationDescriptor::new(
        Epoch::new(0, 4),
        starting.current.primary_id,
        starting.current.members.clone(),
        starting.current.write_quorum,
    );
    PublicOperationProgram::Swap {
        starting,
        refreshed,
        epoch: Epoch::new(0, 4),
        handoff: ReplicaRole::ActiveSecondary,
        catch_up_specific_quorum: false,
        mode: PublicCatchUpMode::All,
        controller_composed,
    }
}

#[tokio::test]
async fn newer_swap_final_epoch_cannot_hide_an_older_or_different_executed_starting_context() {
    for older_epoch in [true, false] {
        let (mut fixture, current) = installed_primary().await;
        let mut starting = current.clone();
        if older_epoch {
            starting.current = ConfigurationDescriptor::new(
                Epoch::new(0, 2),
                current.current.primary_id,
                current.current.members.clone(),
                1,
            );
        } else {
            let peer = ReplicaIdentity {
                replica_id: ReplicaId::new(2),
                instance_id: ReplicaInstanceId::new("peer"),
                agent_generation: AgentGeneration::new("peer-agent"),
            };
            let mut members = current.current.members.clone();
            members.push(ConfigurationMember {
                identity: peer.clone(),
                role: ReplicaRole::ActiveSecondary,
            });
            starting.current = ConfigurationDescriptor::new(
                current.current.epoch,
                current.current.primary_id,
                members,
                1,
            );
            starting.replicas.push(PublicReplicaDescriptor {
                identity: peer.clone(),
                role: ReplicaRole::ActiveSecondary,
                build_id: OperationId::default(),
                replication_address: "http://127.0.0.1:2".into(),
                process_session_id: ProcessSessionId::new("peer-session"),
                storage_incarnation: Some(ReplicaStorageIncarnationContext {
                    replica: peer,
                    storage: ReplicaStorageIdentity::Persistent {
                        pod_uid: PodUid::new("peer-pod"),
                        pvc_uid: PvcUid::new("peer-pvc"),
                    },
                    storage_generation: StorageGeneration::new("peer-storage"),
                }),
                current_progress: 0,
                catch_up_capability: 0,
                is_up: true,
                is_ready: true,
                must_catchup: false,
            });
        }
        let intent = fixture.intent(swap(starting, false), 3);
        intent.validate().unwrap();
        let reservations = Arc::new(AtomicUsize::new(0));
        let observed = reservations.clone();
        fixture.callbacks.access_factory = Some(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::AcqRel);
            None
        }));
        let state = fixture.store.load_state().await.unwrap();
        let events = fixture.probe.events.lock().unwrap().clone();
        assert!(
            matches!(launch(&fixture.registry, fixture.store.clone(), intent, fixture.callbacks.clone()).await,
            Err(HostError::CommandRejected(detail)) if detail.contains(if older_epoch { "older" } else { "starting" }))
        );
        assert_eq!(fixture.store.load_state().await.unwrap(), state);
        assert_eq!(*fixture.probe.events.lock().unwrap(), events);
        assert_eq!(
            reservations.load(Ordering::Acquire),
            0,
            "rejection must precede access reservation"
        );
        assert_eq!(
            *fixture.probe.role.lock().unwrap(),
            Some(ReplicaRole::Primary)
        );
        assert_eq!(
            installed_configuration_for_state(&state, &ProcessSessionId::new("session")),
            Some(current)
        );
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn swap_current_starting_context_executes_but_controller_composed_unused_context_is_not_replayed()
 {
    for composed in [false, true] {
        let (fixture, current) = installed_primary().await;
        let mut starting = current;
        if composed {
            starting.current = ConfigurationDescriptor::new(
                Epoch::new(0, 2),
                starting.current.primary_id,
                starting.current.members.clone(),
                1,
            );
        }

        fixture.probe.events.lock().unwrap().clear();
        let operation = fixture.launch(swap(starting, composed), 3).await;
        assert_eq!(
            completed(&operation).await.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        {
            let events = fixture.probe.events.lock().unwrap();
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.as_str() == "configuration")
                    .count(),
                if composed { 1 } else { 2 }
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event.as_str() == "catchup")
                    .count(),
                if composed { 1 } else { 2 }
            );
        }
        fixture.registry.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn successor_swap_starts_from_receipted_configuration_before_predecessor_role_handoff() {
    let (mut fixture, current) = installed_primary().await;
    let cut = Arc::new(PublicOperationCut {
        position: PublicCutPosition::BeforeInstruction,
        index: 6,
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    fixture.callbacks.cut = Some(cut.clone());
    fixture.callbacks.native_observer_callbacks = true;
    let predecessor = fixture.launch(swap(current.clone(), false), 3).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), cut.entered.notified())
        .await
        .unwrap();
    let state = fixture.store.load_state().await.unwrap();
    assert_eq!(
        installed_configuration_for_state(&state, &ProcessSessionId::new("session")),
        Some(current.clone()),
        "unfinished role handoff must not be reported as completed installation"
    );
    fixture.callbacks.cut = None;
    let stale = fixture.intent(swap(current.clone(), false), 4);
    assert!(matches!(
        launch(&fixture.registry, fixture.store.clone(), stale, fixture.callbacks.clone()).await,
        Err(HostError::CommandRejected(detail)) if detail.contains("older")
    ));
    let mut starting = current;
    starting.current = ConfigurationDescriptor::new(
        Epoch::new(0, 4),
        starting.current.primary_id,
        starting.current.members.clone(),
        starting.current.write_quorum,
    );
    let mut successor = swap(starting, false);
    if let PublicOperationProgram::Swap {
        epoch, refreshed, ..
    } = &mut successor
    {
        *epoch = Epoch::new(0, 5);
        refreshed.current = ConfigurationDescriptor::new(
            *epoch,
            refreshed.current.primary_id,
            refreshed.current.members.clone(),
            refreshed.current.write_quorum,
        );
    }
    let successor = fixture.launch(successor, 4).await;
    assert_eq!(
        completed(&successor).await.disposition,
        Some(PublicOperationDisposition::Succeeded)
    );
    assert_eq!(
        completed(&predecessor).await.disposition,
        Some(PublicOperationDisposition::Cancelled)
    );
    fixture.registry.shutdown().await.unwrap();
}
