use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::FutureExt;
use kuberic_public_v1_tests::{
    CopyContextObservations, CopyStateObservation, ExternalService, PersistedSnapshot,
};
use kuberic_runtime::control::proto;
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
    RunningReplica,
};
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const CREDENTIAL: &str = "public-copy-context-credential";
const BOUND: Duration = Duration::from_secs(20);

fn authorized<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {CREDENTIAL}").parse().unwrap(),
    );
    request
}

fn identity_for(resource: &ResourceUid, id: i64) -> ReplicaIdentity {
    let initialization = derive_initialization_id(
        resource,
        ReplicaId::new(id),
        &PodUid::new(format!("pod-{id}")),
        &PvcUid::new(format!("pvc-{id}")),
    );
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
        agent_generation: derive_agent_generation(&initialization),
    }
}

#[derive(Default)]
struct Resolver(Mutex<BTreeMap<ReplicaId, String>>);
impl ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, identity: &ReplicaIdentity) -> String {
        self.0
            .lock()
            .unwrap()
            .get(&identity.replica_id)
            .cloned()
            .unwrap_or_default()
    }
    fn replication_endpoint(&self, _: &ReplicaIdentity) -> String {
        String::new()
    }
}

struct Node {
    running: RunningReplica,
    service: Arc<ExternalService>,
    identity: ReplicaIdentity,
    resource: ResourceUid,
    control_address: String,
    session: ProcessSessionId,
    revision: u64,
}

impl Node {
    async fn start(
        root: &Path,
        id: i64,
        resolver: Arc<Resolver>,
        advertised: Vec<Bytes>,
        expected: Vec<Bytes>,
    ) -> TestResult<Self> {
        let resource = ResourceUid::new("public-copy-context");
        let identity = identity_for(&resource, id);
        let application = root.join("application");
        assert!(ExternalService::is_fresh(&application)?);
        let service = Arc::new(ExternalService::with_copy_context(
            application.clone(),
            "http://127.0.0.1:0".into(),
            advertised,
            expected,
        ));
        let host = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: resource.clone(),
                replica_id: identity.replica_id,
                pod_uid: PodUid::new(format!("pod-{id}")),
                pvc_uid: PvcUid::new(format!("pvc-{id}")),
                data_root: root.to_path_buf(),
                replication_address: "127.0.0.1:0".parse()?,
                bearer_token: CREDENTIAL.into(),
                rpc_deadline: Duration::from_secs(5),
                transport_window_capacity: 64,
            },
            service.clone(),
            ApplicationStorageState::FreshEmpty,
            resolver.clone(),
        )
        .with_application_storage_paths(BTreeMap::from([("application".into(), application)]));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let control_address = format!("http://{}", listener.local_addr()?);
        resolver
            .0
            .lock()
            .unwrap()
            .insert(identity.replica_id, control_address.clone());
        let mut startup = tokio::spawn(host.start(listener));
        let initialized = tokio::time::timeout(BOUND, async {
            let mut client = loop {
                match proto::agent_control_client::AgentControlClient::connect(
                    control_address.clone(),
                )
                .await
                {
                    Ok(client) => break client,
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            };
            let status = client
                .get_status(authorized(proto::GetAgentStatusRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: resource.to_string(),
                    replica_id: id,
                    expected_instance_id: identity.instance_id.to_string(),
                }))
                .await?
                .into_inner();
            assert_eq!(
                status.storage_state,
                proto::AgentStorageState::Uninitialized as i32
            );
            let initialization = derive_initialization_id(
                &resource,
                identity.replica_id,
                &PodUid::new(format!("pod-{id}")),
                &PvcUid::new(format!("pvc-{id}")),
            );
            let bootstrap = ConfigurationDescriptor::new(
                Epoch::new(0, 1),
                ReplicaId::new(1),
                (1..=id)
                    .map(|member| ConfigurationMember {
                        identity: identity_for(&resource, member),
                        role: if member == 1 {
                            ReplicaRole::Primary
                        } else {
                            ReplicaRole::ActiveSecondary
                        },
                    })
                    .collect(),
                id as u32 / 2 + 1,
            );
            client
                .execute(authorized(proto::ExecuteCommandRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: resource.to_string(),
                    target: Some(identity.clone().into()),
                    expected_process_session_id: status.process_session_id,
                    command: Some(
                        proto::execute_command_request::Command::InitializeAgentStore(
                            proto::InitializeAgentStoreCommand {
                                initialization_id: initialization.to_string(),
                                resource_uid: resource.to_string(),
                                local_replica_id: id,
                                expected_instance_id: identity.instance_id.to_string(),
                                expected_pod_uid: format!("pod-{id}"),
                                expected_pvc_uid: format!("pvc-{id}"),
                                assigned_agent_generation: identity.agent_generation.to_string(),
                                effective_policy: Some(
                                    EffectivePolicy::fixed(id as u32, 1).unwrap().into(),
                                ),
                                bootstrap_configuration: Some(bootstrap.into()),
                                provisioning: None,
                            },
                        ),
                    ),
                }))
                .await?;
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await;
        if let Err(error) = initialized.unwrap_or_else(|error| Err(error.into())) {
            startup.abort();
            let _ = startup.await;
            return Err(error);
        }
        let running = match tokio::time::timeout(BOUND, &mut startup).await {
            Ok(result) => result??,
            Err(error) => {
                startup.abort();
                let _ = startup.await;
                return Err(error.into());
            }
        };
        let mut client =
            proto::agent_control_client::AgentControlClient::connect(control_address.clone())
                .await?;
        let response = client
            .get_snapshot(authorized(proto::ReplicaSnapshotRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                resource_uid: resource.to_string(),
                target: Some(identity.clone().into()),
                expected_process_session_id: String::new(),
                sample_native_progress: false,
            }))
            .await?
            .into_inner();
        let ReplicaAgentQueryResult::Initialized(snapshot) =
            serde_json::from_slice(&response.canonical_json)?
        else {
            return Err("initialized ordinary snapshot required".into());
        };
        let session = snapshot.process_session_id;
        Ok(Self {
            running,
            service,
            identity,
            resource,
            control_address,
            session,
            revision: 0,
        })
    }

    async fn snapshot(&self) -> TestResult<ReplicaAgentSnapshot> {
        let mut client =
            proto::agent_control_client::AgentControlClient::connect(self.control_address.clone())
                .await?;
        let response = client
            .get_snapshot(authorized(proto::ReplicaSnapshotRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                resource_uid: self.resource.to_string(),
                target: Some(self.identity.clone().into()),
                expected_process_session_id: self.session.to_string(),
                sample_native_progress: false,
            }))
            .await?
            .into_inner();
        let ReplicaAgentQueryResult::Initialized(snapshot) =
            serde_json::from_slice(&response.canonical_json)?
        else {
            return Err("initialized ordinary snapshot required".into());
        };
        Ok(*snapshot)
    }

    async fn goal(
        &mut self,
        configuration: ReplicaConfigurationScope,
        desired: ReplicaGoalKind,
    ) -> TestResult<ReplicaGoal> {
        let snapshot = self.snapshot().await?;
        self.revision += 1;
        let goal = ReplicaGoal {
            format: ORDINARY_ACTION_FORMAT,
            identity: snapshot.identity,
            operation_id: OperationId::new(format!(
                "ordinary-{}-{}",
                self.identity.replica_id, self.revision
            )),
            revision: self.revision,
            scope: ReplicaScope {
                history: snapshot.history,
                storage: snapshot.storage,
                process_session_id: self.session.clone(),
                creation_id: snapshot.creation_id,
                epoch: configuration.current.epoch,
                configuration,
            },
            desired,
        };
        let mut client =
            proto::agent_control_client::AgentControlClient::connect(self.control_address.clone())
                .await?;
        client
            .submit_goal(authorized(proto::ReplicaGoalRequest {
                protocol_version: ORDINARY_ACTION_FORMAT,
                canonical_json: serde_json::to_vec(&goal)?,
            }))
            .await?;
        tokio::time::timeout(BOUND, async {
            loop {
                if let Some(observation) = self
                    .snapshot()
                    .await?
                    .goals
                    .iter()
                    .find(|observation| observation.goal == goal)
                {
                    match &observation.status {
                        ReplicaGoalStatus::Achieved => {
                            return Ok::<_, Box<dyn std::error::Error + Send + Sync>>(());
                        }
                        ReplicaGoalStatus::Pending => {}
                        status => return Err(format!("ordinary goal failed: {status:?}").into()),
                    }
                }
                tokio::task::yield_now().await;
            }
        })
        .await??;
        Ok(goal)
    }

    async fn close(mut self, configuration: ReplicaConfigurationScope) -> TestResult {
        let closed = self.goal(configuration, ReplicaGoalKind::Close).await;
        self.running.shutdown();
        let waited = tokio::time::timeout(BOUND, self.running.wait()).await;
        let drained = tokio::time::timeout(BOUND, self.service.shutdown_and_drain()).await;
        closed?;
        waited??;
        drained??;
        Ok(())
    }
}

#[tokio::test]
async fn public_build_consumes_target_context_and_installs_nonempty_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let target_application = root.path().join("target/application");
    let resolver = Arc::new(Resolver::default());
    let target_context = vec![
        Bytes::from_static(b"target/context"),
        Bytes::new(),
        Bytes::from_static(b"target/trailer"),
    ];
    let source_context = vec![
        Bytes::from_static(b"source/context"),
        Bytes::new(),
        Bytes::from_static(b"source/trailer"),
    ];
    let expected_values = vec![
        Bytes::from_static(b"application/committed-first"),
        Bytes::from_static(b"application/committed-second"),
    ];
    let mut source = Node::start(
        &root.path().join("source"),
        1,
        resolver.clone(),
        source_context.clone(),
        target_context.clone(),
    )
    .await
    .unwrap();
    let mut target = Node::start(
        &root.path().join("target"),
        2,
        resolver,
        target_context.clone(),
        Vec::new(),
    )
    .await
    .unwrap();
    let current = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        source.identity.replica_id,
        vec![ConfigurationMember {
            identity: source.identity.clone(),
            role: ReplicaRole::Primary,
        }],
        1,
    );
    let configuration = ReplicaConfigurationScope {
        current: current.clone(),
        previous: None,
        replicas: Vec::new(),
    };
    let mut copied_snapshot = None;
    let outcome = std::panic::AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(90), async {
        source.goal(configuration.clone(), ReplicaGoalKind::Prepare { role: ReplicaRole::Primary }).await?;
        let before_access = source.snapshot().await?;
        assert_ne!(before_access.write_status, AccessStatus::Granted);
        assert!(source.service.begin_write(Bytes::from_static(b"forbidden-before-access")).await.is_err());
        source.goal(configuration.clone(), ReplicaGoalKind::Reconfigure { role: ReplicaRole::Primary, catch_up: Some(PublicCatchUpMode::WriteQuorum) }).await?;
        let mut committed = 0;
        for value in &expected_values {
            let write = source.service.begin_write(value.clone()).await?;
            let assigned = write.lsn;
            committed = write.wait().await?;
            assert_eq!(committed, assigned);
        }
        assert!(committed > 0);
        assert_eq!(source.service.values().await?, expected_values);
        target.goal(configuration.clone(), ReplicaGoalKind::Prepare { role: ReplicaRole::IdleSecondary }).await?;
        assert_eq!(ExternalService::read_persisted_snapshot(&target_application)?, PersistedSnapshot { committed_lsn: 0, values: Vec::new() });
        assert_eq!(target.service.copy_context_observations(), CopyContextObservations::default());
        let source_publication = source.snapshot().await?.publication.unwrap();
        let target_publication = target.snapshot().await?.publication.unwrap();
        assert_eq!(source_publication.stage, ReplicaPublicationStage::AchievedScope);
        assert_eq!(target_publication.stage, ReplicaPublicationStage::AchievedScope);
        let remote = RemoteReplicaSelection {
            source: source_publication, target: target_publication,
            target_control_address: target.control_address.clone(), generation: 1,
        };
        let build = source.goal(configuration.clone(), ReplicaGoalKind::Build { remote: Box::new(remote.clone()) }).await?;
        let ready = source.snapshot().await?;
        assert!(ready.goals.iter().any(|entry| entry.goal == build && entry.status == ReplicaGoalStatus::Achieved));
        assert!(ready.remote_operations.iter().any(|entry| entry.operation_id == build.operation_id
            && entry.selection == remote && entry.state == RemoteReplicaState::Ready && !entry.accepted));
        assert_eq!(ready.publication.unwrap().scope.configuration.current, current);
        assert!(!current.members.iter().any(|member| member.identity == target.identity));
        assert_eq!(source.service.copy_context_observations(), CopyContextObservations {
            context_acquisitions: 0,
            copy_state_acquisitions: vec![CopyStateObservation { up_to: committed, items: target_context.clone(), reached_eof: true }],
        });
        assert_ne!(target_context, source_context);
        assert_eq!(target.service.copy_context_observations(), CopyContextObservations { context_acquisitions: 1, copy_state_acquisitions: Vec::new() });
        let snapshot = ExternalService::read_persisted_snapshot(&target_application)?;
        assert_eq!(snapshot, PersistedSnapshot { committed_lsn: committed, values: expected_values.clone() });
        assert_ne!(snapshot.values, target_context);
        let idle = target.snapshot().await?;
        assert_eq!(idle.publication.unwrap().role, ReplicaRole::IdleSecondary);
        assert_ne!(idle.read_status, AccessStatus::Granted);
        assert_ne!(idle.write_status, AccessStatus::Granted);
        assert!(matches!(target.service.values().await, Err(kuberic_runtime::RuntimeError::ReadClosed(_))));
        assert!(target.service.begin_write(Bytes::from_static(b"forbidden-idle")).await.is_err());
        copied_snapshot = Some(snapshot);
        Ok::<_, Box<dyn std::error::Error + Send + Sync>>(())
    })).catch_unwind().await;
    let (source_closed, target_closed) = tokio::join!(
        source.close(configuration.clone()),
        target.close(configuration)
    );
    assert!(source_closed.is_ok(), "{source_closed:?}");
    assert!(target_closed.is_ok(), "{target_closed:?}");
    match outcome {
        Ok(result) => result.expect("bounded ordinary external copy").unwrap(),
        Err(panic) => std::panic::resume_unwind(panic),
    }
    assert_eq!(
        ExternalService::read_persisted_snapshot(&target_application).unwrap(),
        copied_snapshot.unwrap()
    );
}
