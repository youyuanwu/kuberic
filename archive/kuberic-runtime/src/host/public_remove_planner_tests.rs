//! Synthetic callback contract fixtures, not native-engine qualification.
//! Commands come from the production planners and execute through the real
//! registry, SQLite store and host lifecycle dispatcher without seeded receipts.

use super::*;
use crate::protocol::command::PublicV1OperationCommand;
use crate::protocol::public_operations::{
    ProcessSessionRenewalRequired, ProgressQueryResult, PublicOperationClass, PublicProgressRole,
    PublicV1AcceptedOperationCompletion, PublicV1AcceptedStatus, PublicV1EngineIdleObservation,
    PublicV1Extensions, PublicV1SessionRenewalStatus,
};
use kuberic_controller::evaluator::public_v1_topology::{
    PublicV1TopologyPlan, PublicV1TopologyReplica, plan_public_v1_topology,
};
use serde::{Serialize, de::DeserializeOwned};

#[tokio::test]
async fn planner_remove_preserves_authority_revision_after_unrelated_idle_retirement() {
    for planner in [Planner::Topology, Planner::Renewal] {
        for diagnostic in [false, true] {
            let journey = Journey::new().await;
            let (build, _) = journey.start(BuildState::Completed).await;
            // The synthetic backend permits idempotent removal of a native-only
            // idle slot. Its host program is Authority even though completion
            // call_kind is Remove; the still-owned Build for replica 2 survives.
            let retirement = journey
                .source
                .launch(
                    PublicOperationProgram::RetireIdleReplica {
                        replica_id: ReplicaId::new(3),
                    },
                    4,
                )
                .await;
            assert_eq!(
                completed(&retirement).await.disposition,
                Some(PublicOperationDisposition::Succeeded)
            );
            let command = journey.planned(planner, diagnostic).await.unwrap();
            assert_eq!(command.intent.revision, 4);
            let Some(PublicOperationProgram::Build(input)) = build.program else {
                unreachable!()
            };
            assert_eq!(
                command.intent.program,
                Some(PublicOperationProgram::Remove(input))
            );
            let operation = journey.dispatch(command.intent.clone()).await;
            let result = completed(&operation).await;
            assert_eq!(
                result.disposition,
                Some(PublicOperationDisposition::Succeeded)
            );
            let duplicate = journey.dispatch(command.intent).await;
            assert_eq!(completed(&duplicate).await, result);
            assert_eq!(
                *journey.remote.removals.lock().unwrap(),
                vec![ReplicaId::new(3), ReplicaId::new(2)]
            );
            journey.shutdown().await;
        }
    }
}

#[derive(Default)]
struct RemoteProbe {
    builds: AtomicUsize,
    removals: Mutex<Vec<ReplicaId>>,
    block: AtomicBool,
    fail: AtomicBool,
    entered: Notify,
}

struct RemotePrimary(Arc<Probe>, Arc<RemoteProbe>);

#[async_trait::async_trait]
impl Replicator for RemotePrimary {
    async fn open(&self) -> crate::Result<String> {
        Replication(self.0.clone()).open().await
    }
    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> crate::Result<()> {
        Replication(self.0.clone()).change_role(epoch, role).await
    }
    async fn update_epoch(&self, epoch: Epoch) -> crate::Result<()> {
        Replication(self.0.clone()).update_epoch(epoch).await
    }
    async fn close(&self) -> crate::Result<()> {
        Replication(self.0.clone()).close().await
    }
    fn abort(&self) {
        Replication(self.0.clone()).abort();
    }
    async fn current_progress(&self) -> crate::Result<i64> {
        Replication(self.0.clone()).current_progress().await
    }
    async fn catch_up_capability(&self) -> crate::Result<i64> {
        Replication(self.0.clone()).catch_up_capability().await
    }
}

#[async_trait::async_trait]
impl PrimaryReplicator for RemotePrimary {
    async fn on_data_loss(&self) -> crate::Result<bool> {
        Ok(false)
    }

    async fn update_catch_up_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
        previous: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        Replication(self.0.clone())
            .update_catch_up_replica_set_configuration(current, previous)
            .await
    }

    async fn update_current_replica_set_configuration(
        &self,
        configuration: ReplicaSetConfiguration,
    ) -> crate::Result<()> {
        Replication(self.0.clone())
            .update_current_replica_set_configuration(configuration)
            .await
    }

    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> crate::Result<()> {
        Replication(self.0.clone())
            .wait_for_catch_up_quorum(mode)
            .await
    }

    async fn build_replica(&self, _: ReplicaInformation) -> crate::Result<()> {
        self.1.builds.fetch_add(1, Ordering::AcqRel);
        self.1.entered.notify_one();
        if self.1.block.load(Ordering::Acquire) {
            std::future::pending::<()>().await;
        }
        if self.1.fail.load(Ordering::Acquire) {
            return Err(RuntimeError::Application("uncertain Build fixture".into()));
        }
        Ok(())
    }

    async fn remove_replica(&self, replica: ReplicaId) -> crate::Result<()> {
        self.1.removals.lock().unwrap().push(replica);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum Planner {
    Topology,
    Renewal,
}

#[derive(Clone, Copy, Debug)]
enum BuildState {
    Active,
    Completed,
    FailedContained,
}

// Unit-test and ordinary runtime artifacts have distinct Rust type identities
// across the controller dev-dependency. Use the actual serialized control types,
// rather than duplicate a planner or manufacture its output.
fn wire<T: Serialize, U: DeserializeOwned>(input: &T) -> U {
    serde_json::from_value(serde_json::to_value(input).unwrap()).unwrap()
}

async fn target_fixture() -> Fixture {
    let mut fixture = Fixture::new().await;
    fixture.registry.shutdown().await.unwrap();
    let state = fixture.store.load_state().await.unwrap();
    let preview = state.public_v1_operation_state.unwrap();
    let mut identity = state.identity;
    identity.local_identity = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: ReplicaInstanceId::new("target"),
        agent_generation: AgentGeneration::new("target-agent"),
    };
    identity.pod_uid = PodUid::new("target");
    identity.pvc_uid = PvcUid::new("target-pvc");
    let storage = ReplicaStorageIncarnationContext {
        replica: identity.local_identity.clone(),
        storage: ReplicaStorageIdentity::Persistent {
            pod_uid: identity.pod_uid.clone(),
            pvc_uid: identity.pvc_uid.clone(),
        },
        storage_generation: StorageGeneration::new("target-storage"),
    };
    fixture.store = Arc::new(
        SqliteStore::create_replacement_authorized(
            fixture._root.path().join("target.sqlite3"),
            AgentState::new(identity),
            preview.binding.unwrap(),
        )
        .unwrap(),
    );
    fixture
        .store
        .initialize_public_v1_history(
            &preview.history_initialization_id.unwrap(),
            &preview.history.unwrap(),
            &storage,
        )
        .await
        .unwrap();
    fixture.registry = PartitionOperationRegistry::new(
        fixture.store.clone(),
        PublicV1Identity::new(6),
        ProcessSessionId::new("session"),
    )
    .unwrap();
    fixture
}

async fn report(fixture: &Fixture, role: ReplicaRole, cold: bool) -> PublicV1AgentReport {
    let state = fixture.store.load_state().await.unwrap();
    let history = state.public_v1_operation_state.unwrap().history.unwrap();
    let progress = if cold {
        ProgressQueryResult::NotQueried("synthetic cold target".into())
    } else {
        ProgressQueryResult::Value(0)
    };
    public_v1_report(
        fixture.store.as_ref(),
        &PublicV1Identity::new(6),
        PublicV1ReportProjection {
            catch_up_specific_quorum: Some(false),
            process_session_id: ProcessSessionId::new("session"),
            replicator_creation_id: ReplicatorCreationId::new("creation"),
            report_sequence: 1,
            role,
            read_status: AccessStatus::NotPrimary,
            write_status: AccessStatus::NotPrimary,
            progress: PublicProgress {
                role: if role == ReplicaRole::Primary {
                    PublicProgressRole::Primary
                } else {
                    PublicProgressRole::Secondary
                },
                current_progress: progress.clone(),
                catch_up_capability: progress,
            },
            progress_history: Some(history),
            engine_idle: None,
            observation_alive: None,
        },
    )
    .await
    .unwrap()
}

struct Journey {
    source: Fixture,
    target: Fixture,
    remote: Arc<RemoteProbe>,
    contained: tokio::sync::watch::Sender<bool>,
    configuration: PublicConfiguration,
}

impl Journey {
    async fn new() -> Self {
        let mut source = Fixture::new().await;
        let mut target = target_fixture().await;
        let remote = Arc::new(RemoteProbe::default());
        source.callbacks.primary = Arc::new(RemotePrimary(source.probe.clone(), remote.clone()));
        let (contained, witness) = tokio::sync::watch::channel(false);
        source.callbacks.containment = Some(witness);
        source.callbacks.wait_for_containment = true;
        for fixture in [&mut source, &mut target] {
            let replica = fixture
                .bind_open_context(crate::application::OpenMode::Existing)
                .await;
            let operation_id = OperationId::new(format!("contract-open-{}", replica.replica_id));
            let mut intent = fixture.intent(
                PublicOperationProgram::Open {
                    replica,
                    existing: true,
                },
                1,
            );
            intent.operation_id = operation_id;
            let open = launch(
                &fixture.registry,
                fixture.store.clone(),
                intent,
                fixture.callbacks.clone(),
            )
            .await
            .unwrap();
            assert_eq!(
                completed(&open).await.disposition,
                Some(PublicOperationDisposition::Succeeded)
            );
        }
        for (fixture, role) in [
            (&source, ReplicaRole::Primary),
            (&target, ReplicaRole::IdleSecondary),
        ] {
            let mut intent = fixture.intent(
                PublicOperationProgram::Role {
                    epoch: Epoch::new(0, 2),
                    role,
                },
                2,
            );
            intent.operation_id = OperationId::new(format!("contract-role-{role:?}"));
            let operation = launch(
                &fixture.registry,
                fixture.store.clone(),
                intent,
                fixture.callbacks.clone(),
            )
            .await
            .unwrap();
            assert_eq!(
                completed(&operation).await.disposition,
                Some(PublicOperationDisposition::Succeeded)
            );
        }
        let local = source
            .store
            .load_state()
            .await
            .unwrap()
            .identity
            .local_identity;
        let configuration = PublicConfiguration {
            current: ConfigurationDescriptor::new(
                Epoch::new(0, 2),
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
        let installed = source
            .launch(
                PublicOperationProgram::Configuration(configuration.clone()),
                3,
            )
            .await;
        assert_eq!(
            completed(&installed).await.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        Self {
            source,
            target,
            remote,
            contained,
            configuration,
        }
    }

    async fn build(&self, id: &str, revision: u64) -> PublicOperationIntent {
        let source = report(&self.source, ReplicaRole::Primary, false).await;
        let target = report(&self.target, ReplicaRole::IdleSecondary, true).await;
        let input = PublicBuildInput {
            attempt: OperationId::new(id),
            source_address: source.replication_address.unwrap(),
            epoch: self.configuration.current.epoch,
            current_configuration_id: Some(self.configuration.current.configuration_id.clone()),
            replica: target.replica,
            target_storage: target.storage_incarnation,
            process_session_id: target.process_session_id,
            replication_address: target.replication_address.unwrap(),
            history: Some(source.history),
        };
        let mut intent = self
            .source
            .intent(PublicOperationProgram::Build(input.clone()), revision);
        intent.operation_id = input.attempt;
        intent.class = PublicOperationClass::Build {
            target: input.replica.replica_id,
        };
        intent.validate().unwrap();
        intent
    }

    async fn dispatch(&self, intent: PublicOperationIntent) -> Arc<PartitionOperation> {
        launch(
            &self.source.registry,
            self.source.store.clone(),
            intent,
            self.source.callbacks.clone(),
        )
        .await
        .unwrap()
    }

    async fn start(&self, mode: BuildState) -> (PublicOperationIntent, Arc<PartitionOperation>) {
        self.remote
            .block
            .store(matches!(mode, BuildState::Active), Ordering::Release);
        self.remote.fail.store(
            matches!(mode, BuildState::FailedContained),
            Ordering::Release,
        );
        let intent = self.build("predecessor-build", 3).await;
        let operation = self.dispatch(intent.clone()).await;
        self.remote.entered.notified().await;
        match mode {
            BuildState::Active => {
                assert_eq!(operation.snapshot().stage, PublicOperationStage::Running)
            }
            BuildState::Completed => {
                assert_eq!(
                    completed(&operation).await.disposition,
                    Some(PublicOperationDisposition::Succeeded)
                );
            }
            BuildState::FailedContained => {
                wait_stage(&operation, PublicOperationStage::ContainmentPending).await;
                assert!(operation.complete_containment().await.is_err());
                self.contained.send_replace(true);
                operation.complete_containment().await.unwrap();
                assert!(matches!(
                    completed(&operation).await.disposition,
                    Some(PublicOperationDisposition::Failed(_))
                ));
            }
        }
        (intent, operation)
    }

    async fn planned(
        &self,
        planner: Planner,
        diagnostic: bool,
    ) -> std::result::Result<PublicV1OperationCommand, String> {
        for _ in 0..3 {
            let mut source = report(&self.source, ReplicaRole::Primary, false).await;
            let target = report(&self.target, ReplicaRole::IdleSecondary, true).await;
            if diagnostic {
                source.engine_idle = Some(PublicV1EngineIdleObservation {
                    configuration: source.configuration.as_ref().unwrap().voting.clone(),
                    history: source.history.clone(),
                    storage_incarnation: source.storage_incarnation.clone(),
                    process_session_id: source.process_session_id.clone(),
                    replicator_creation_id: source.replicator_creation_id.clone(),
                    idle_replicas: vec![target.replica.replica_id],
                });
            }
            source.validate().unwrap();
            let state = self.source.store.load_state().await.unwrap();
            let authority = PublicV1AcceptedStatus {
                identity: source.identity.clone(),
                history_initialization_id: state
                    .public_v1_operation_state
                    .unwrap()
                    .history_initialization_id
                    .unwrap(),
                history: source.history.clone(),
                data_loss_recovery: None,
                completed_operations: source
                    .completed_operations
                    .iter()
                    .chain(&target.completed_operations)
                    .map(PublicV1AcceptedOperationCompletion::from_completion)
                    .collect(),
                extensions: PublicV1Extensions::default(),
            };
            let replicas = vec![
                PublicV1TopologyReplica {
                    replication_address: source.replication_address.clone().unwrap(),
                    report: wire(&source),
                },
                PublicV1TopologyReplica {
                    replication_address: target.replication_address.clone().unwrap(),
                    report: wire(&target),
                },
            ];
            let command = match planner {
                Planner::Topology => {
                    let PublicV1TopologyPlan::Begin { command, .. } = plan_public_v1_topology(
                        &wire(&authority),
                        Some(&wire(&self.configuration.current)),
                        &replicas,
                        2,
                        None,
                        Some(wire(&source.replica.replica_id)),
                        "remove-contract-resource",
                    )?
                    else {
                        panic!("exact retained Build must be resolved before new Build");
                    };
                    wire::<_, PublicV1OperationCommand>(&command)
                }
                Planner::Renewal => {
                    let endpoint = |report: &PublicV1AgentReport, role| ProcessSessionEndpoint {
                        identity: report.replica.clone(),
                        storage_incarnation: report.storage_incarnation.clone(),
                        process_session_id: report.process_session_id.clone(),
                        replicator_creation_id: report.replicator_creation_id.clone(),
                        role,
                    };
                    let mut predecessor = endpoint(&target, ReplicaRole::ActiveSecondary);
                    predecessor.process_session_id = ProcessSessionId::new("predecessor-session");
                    predecessor.replicator_creation_id =
                        ReplicatorCreationId::new("predecessor-creation");
                    let replacement = endpoint(&target, ReplicaRole::None);
                    let renewal_epoch = Epoch::new(
                        self.configuration.current.epoch.data_loss_number,
                        self.configuration.current.epoch.configuration_number + 1,
                    );
                    let renewal =
                        PublicV1SessionRenewalStatus::required(ProcessSessionRenewalRequired {
                            operation_id: OperationId::new("synthetic-renewal-contract"),
                            reason: ProcessSessionRenewalReason::ActiveSecondaryReplacement,
                            resource_uid: authority.history.resource_uid.clone(),
                            history: authority.history.clone(),
                            previous_epoch: self.configuration.current.epoch,
                            renewal_epoch,
                            predecessor,
                            replacement: Some(replacement),
                            surviving_sessions: vec![endpoint(&source, ReplicaRole::Primary)],
                            fresh_configuration: ConfigurationDescriptor::new(
                                renewal_epoch,
                                source.replica.replica_id,
                                vec![
                                    ConfigurationMember {
                                        identity: source.replica.clone(),
                                        role: ReplicaRole::Primary,
                                    },
                                    ConfigurationMember {
                                        identity: target.replica.clone(),
                                        role: ReplicaRole::ActiveSecondary,
                                    },
                                ],
                                2,
                            ),
                            predecessor_contained: true,
                            data_loss: None,
                        })
                        .unwrap();
                    let command =
                        kuberic_controller::cluster_api::plan_public_v1_renewal_preparation(
                            &wire(&authority),
                            &wire(&renewal),
                            &replicas,
                        )?
                        .expect("renewal must resolve exact retained Build");
                    let kuberic_controller::protocol::command::ProtocolCommand::PublicV1Operation(
                        command,
                    ) = command
                    else {
                        panic!("renewal retirement must dispatch a public operation");
                    };
                    wire::<_, PublicV1OperationCommand>(&command)
                }
            };
            if matches!(
                command.intent.program,
                Some(PublicOperationProgram::Role {
                    role: ReplicaRole::IdleSecondary,
                    ..
                })
            ) {
                assert_eq!(command.target, target.replica);
                let prepared = launch(
                    &self.target.registry,
                    self.target.store.clone(),
                    command.intent,
                    self.target.callbacks.clone(),
                )
                .await
                .unwrap();
                assert_eq!(
                    completed(&prepared).await.disposition,
                    Some(PublicOperationDisposition::Succeeded)
                );
                continue;
            }
            assert_eq!(command.target, source.replica);
            assert!(matches!(
                command.intent.program,
                Some(PublicOperationProgram::Remove(_))
            ));
            return Ok(command);
        }
        panic!("planner repeated completed target preparation");
    }

    async fn advance(&mut self) {
        let epoch = Epoch::new(0, 3);
        let advanced = self
            .source
            .launch(PublicOperationProgram::Epoch { epoch }, 4)
            .await;
        assert_eq!(
            completed(&advanced).await.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
        self.configuration.current = ConfigurationDescriptor::new(
            epoch,
            self.configuration.current.primary_id,
            self.configuration.current.members.clone(),
            1,
        );
        let installed = self
            .source
            .launch(
                PublicOperationProgram::Configuration(self.configuration.clone()),
                5,
            )
            .await;
        assert_eq!(
            completed(&installed).await.disposition,
            Some(PublicOperationDisposition::Succeeded)
        );
    }

    async fn shutdown(&self) {
        self.source.registry.shutdown().await.unwrap();
        self.target.registry.shutdown().await.unwrap();
    }
}

async fn wait_stage(operation: &PartitionOperation, expected: PublicOperationStage) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while operation.snapshot().stage != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn planner_remove_uses_build_revision_and_real_containment_without_diagnostics() {
    for planner in [Planner::Topology, Planner::Renewal] {
        for diagnostic in [false, true] {
            for mode in [
                BuildState::Active,
                BuildState::Completed,
                BuildState::FailedContained,
            ] {
                let journey = Journey::new().await;
                let (build, predecessor) = journey.start(mode).await;
                let command = journey.planned(planner, diagnostic).await.unwrap();
                assert_eq!(command.intent.revision, build.revision);
                let Some(PublicOperationProgram::Build(input)) = &build.program else {
                    unreachable!()
                };
                assert_eq!(
                    command.intent.program,
                    Some(PublicOperationProgram::Remove(input.clone()))
                );
                let remove = journey.dispatch(command.intent.clone()).await;
                if matches!(mode, BuildState::Active) {
                    wait_stage(&predecessor, PublicOperationStage::ContainmentPending).await;
                    assert_eq!(
                        remove.snapshot().stage,
                        PublicOperationStage::WaitingForContainment
                    );
                    assert!(journey.remote.removals.lock().unwrap().is_empty());
                    let attached = journey.dispatch(command.intent.clone()).await;
                    assert!(Arc::ptr_eq(&remove, &attached));
                    assert!(predecessor.complete_containment().await.is_err());
                    journey.contained.send_replace(true);
                }
                let result = completed(&remove).await;
                assert_eq!(
                    result.disposition,
                    Some(PublicOperationDisposition::Succeeded)
                );
                let duplicate = journey.dispatch(command.intent).await;
                assert_eq!(completed(&duplicate).await, result);
                assert_eq!(
                    *journey.remote.removals.lock().unwrap(),
                    vec![ReplicaId::new(2)]
                );
                assert_eq!(journey.remote.builds.load(Ordering::Acquire), 1);
                let state = journey.source.store.load_state().await.unwrap();
                let preview = state.public_v1_operation_state.unwrap();
                assert!(preview.active_builds.is_empty());
                assert_eq!(
                    preview.build_records[&build.operation_id].retirement,
                    crate::host::public_build_store::PublicV1BuildRetirement::Retired
                );
                journey.shutdown().await;
            }
        }
    }
}

#[tokio::test]
async fn planner_remove_at_current_epoch_withdraws_exact_old_build_and_fences_successor() {
    for planner in [Planner::Topology, Planner::Renewal] {
        for diagnostic in [false, true] {
            for contained in [false, true] {
                let mut journey = Journey::new().await;
                let (build, predecessor) = journey
                    .start(if contained {
                        BuildState::Active
                    } else {
                        BuildState::Completed
                    })
                    .await;
                if contained {
                    let authority = journey
                        .source
                        .launch(
                            PublicOperationProgram::Epoch {
                                epoch: Epoch::new(0, 3),
                            },
                            4,
                        )
                        .await;
                    wait_stage(&predecessor, PublicOperationStage::ContainmentPending).await;
                    assert!(journey.planned(planner, diagnostic).await.is_err());
                    assert!(journey.remote.removals.lock().unwrap().is_empty());
                    assert_eq!(journey.remote.builds.load(Ordering::Acquire), 1);
                    journey.contained.send_replace(true);
                    completed(&predecessor).await;
                    assert_eq!(
                        completed(&authority).await.disposition,
                        Some(PublicOperationDisposition::Succeeded)
                    );
                }
                journey.advance().await;
                let command = journey.planned(planner, diagnostic).await.unwrap();
                assert_eq!(command.intent.revision, 5);
                let Some(PublicOperationProgram::Build(old)) = &build.program else {
                    unreachable!()
                };
                assert_eq!(old.epoch, Epoch::new(0, 2));
                assert_eq!(
                    command.intent.program,
                    Some(PublicOperationProgram::Remove(old.clone()))
                );
                let removed = journey.dispatch(command.intent.clone()).await;
                let result = completed(&removed).await;
                assert_eq!(
                    result.disposition,
                    Some(PublicOperationDisposition::Succeeded)
                );

                journey.remote.block.store(false, Ordering::Release);
                let mut successor = journey.build("successor-build", 5).await;
                let Some(PublicOperationProgram::Build(successor_input)) =
                    successor.program.as_mut()
                else {
                    unreachable!()
                };
                // Exact Remove retires the old storage incarnation as well as the
                // attempt. The successor represents a replacement, not its reuse.
                successor_input.target_storage.storage_generation =
                    StorageGeneration::new("successor-storage");
                successor_input.process_session_id = ProcessSessionId::new("successor-session");
                successor.input_digest = successor.program.as_ref().unwrap().digest();
                let succeeded = journey.dispatch(successor.clone()).await;
                assert_eq!(
                    completed(&succeeded).await.disposition,
                    Some(PublicOperationDisposition::Succeeded)
                );
                let replay = journey.dispatch(command.intent.clone()).await;
                assert_eq!(completed(&replay).await, result);
                let mut stale_build = successor.clone();
                stale_build.operation_id = OperationId::new("stale-new-build");
                let Some(PublicOperationProgram::Build(stale_input)) = stale_build.program.as_mut()
                else {
                    unreachable!()
                };
                stale_input.attempt = stale_build.operation_id.clone();
                stale_input.epoch = old.epoch;
                stale_build.input_digest = stale_build.program.as_ref().unwrap().digest();
                assert!(matches!(
                    launch(
                        &journey.source.registry,
                        journey.source.store.clone(),
                        stale_build,
                        journey.source.callbacks.clone(),
                    ).await,
                    Err(HostError::CommandRejected(message)) if message.contains("older than accepted")
                ));
                let mut mutations = vec![old.clone(); 6];
                mutations[1].epoch = Epoch::new(0, 3);
                mutations[2].attempt = OperationId::new("changed-attempt");
                mutations[3].process_session_id = ProcessSessionId::new("changed-target-session");
                mutations[4].target_storage.storage_generation =
                    StorageGeneration::new("changed-storage");
                mutations[5].history.as_mut().unwrap().history_id =
                    HistoryId::new("changed-history");
                for (index, stale) in mutations.into_iter().enumerate() {
                    let mut intent = command.intent.clone();
                    intent.operation_id = OperationId::new(format!("stale-remove-{index}"));
                    intent.program = Some(PublicOperationProgram::Remove(stale));
                    intent.input_digest = intent.program.as_ref().unwrap().digest();
                    assert!(
                        launch(
                            &journey.source.registry,
                            journey.source.store.clone(),
                            intent,
                            journey.source.callbacks.clone(),
                        )
                        .await
                        .is_err(),
                        "stale mutation {index} reached successor"
                    );
                }
                for revision in [4, 6] {
                    let mut stale = command.intent.clone();
                    stale.operation_id = OperationId::new(format!("stale-revision-{revision}"));
                    stale.revision = revision;
                    let Some(PublicOperationProgram::Build(current)) = &successor.program else {
                        unreachable!()
                    };
                    stale.program = Some(PublicOperationProgram::Remove(current.clone()));
                    stale.input_digest = stale.program.as_ref().unwrap().digest();
                    assert!(matches!(
                        launch(
                            &journey.source.registry,
                            journey.source.store.clone(),
                            stale,
                            journey.source.callbacks.clone(),
                        )
                        .await,
                        Err(HostError::CommandRejected(message)) if message.contains("revision")
                    ));
                }
                let mut stale = command.intent.clone();
                stale.operation_id = OperationId::new("stale-source-session");
                stale.process_session_id = ProcessSessionId::new("stale-source");
                assert!(
                    launch(
                        &journey.source.registry,
                        journey.source.store.clone(),
                        stale,
                        journey.source.callbacks.clone(),
                    )
                    .await
                    .is_err()
                );
                let state = journey.source.store.load_state().await.unwrap();
                let preview = state.public_v1_operation_state.unwrap();
                let Some(PublicOperationProgram::Build(current)) = successor.program else {
                    unreachable!()
                };
                assert_eq!(preview.active_builds[&ReplicaId::new(2)], current);
                assert_eq!(journey.remote.removals.lock().unwrap().len(), 1);
                assert_eq!(journey.remote.builds.load(Ordering::Acquire), 2);
                let mut cleanup = command.intent;
                cleanup.operation_id = OperationId::new("exact-successor-remove");
                cleanup.program = Some(PublicOperationProgram::Remove(current));
                cleanup.input_digest = cleanup.program.as_ref().unwrap().digest();
                let resolved = journey.dispatch(cleanup.clone()).await;
                assert_eq!(
                    completed(&resolved).await.disposition,
                    Some(PublicOperationDisposition::Succeeded)
                );
                let duplicate = journey.dispatch(cleanup).await;
                assert_eq!(completed(&duplicate).await, resolved.snapshot());
                assert_eq!(journey.remote.removals.lock().unwrap().len(), 2);
                journey.shutdown().await;
            }
        }
    }
}
