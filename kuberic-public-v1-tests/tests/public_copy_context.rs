use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures::FutureExt;
use kuberic_public_v1_tests::{
    CopyContextObservations, CopyStateObservation, ExternalService, PersistedSnapshot,
};
use kuberic_runtime::control::{convert, proto};
use kuberic_runtime::host::{
    ApplicationStorageState, ReplicaEndpointResolver, ReplicaHost, ReplicaProcessConfig,
    RunningReplica,
};
use kuberic_runtime::protocol::command::{PublicV1HistoryCommand, PublicV1OperationCommand};
use kuberic_runtime::protocol::public_operations::*;
use kuberic_runtime::protocol::types::*;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const CREDENTIAL: &str = "public-copy-context-credential";
const RPC_BOUND: Duration = Duration::from_secs(20);

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

async fn report_at(
    address: &str,
    resource: &ResourceUid,
    identity: &ReplicaIdentity,
) -> TestResult<PublicV1AgentReport> {
    tokio::time::timeout(RPC_BOUND, async {
        let mut client =
            proto::agent_control_client::AgentControlClient::connect(address.to_owned()).await?;
        let response = client
            .get_status(authorized(proto::GetAgentStatusRequest {
                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                resource_uid: resource.to_string(),
                replica_id: identity.replica_id.value(),
                expected_instance_id: identity.instance_id.to_string(),
            }))
            .await?
            .into_inner();
        Ok(convert::normalize_public_v1_agent_status_report(response)?)
    })
    .await?
}

struct Resolver;

impl ReplicaEndpointResolver for Resolver {
    fn control_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }

    fn replication_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

struct Node {
    running: RunningReplica,
    service: Arc<ExternalService>,
    identity: ReplicaIdentity,
    storage: ReplicaStorageIncarnationContext,
    history: HistoryContext,
    revision: u64,
    control_address: String,
    session: ProcessSessionId,
}

impl Node {
    async fn start(
        root: &Path,
        id: i64,
        replicas: u32,
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
            Arc::new(Resolver),
        )
        .with_application_storage_paths(std::collections::BTreeMap::from([(
            "application".into(),
            application,
        )]));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let control_address = format!("http://{}", listener.local_addr()?);
        let mut startup = tokio::spawn(host.start(listener));
        let initialized = tokio::time::timeout(RPC_BOUND, async {
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
            if status.storage_state != proto::AgentStorageState::Uninitialized as i32 {
                return Err("fresh host unexpectedly had initialized metadata".into());
            }
            let initialization = derive_initialization_id(
                &resource,
                identity.replica_id,
                &PodUid::new(format!("pod-{id}")),
                &PvcUid::new(format!("pvc-{id}")),
            );
            let bootstrap = ConfigurationDescriptor::new(
                Epoch::new(0, 1),
                ReplicaId::new(1),
                (1..=i64::from(replicas))
                    .map(|member| ConfigurationMember {
                        identity: identity_for(&resource, member),
                        role: if member == 1 {
                            ReplicaRole::Primary
                        } else {
                            ReplicaRole::ActiveSecondary
                        },
                    })
                    .collect(),
                replicas / 2 + 1,
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
                                    EffectivePolicy::fixed(replicas, 1)
                                        .ok_or("invalid replica policy")?
                                        .into(),
                                ),
                                bootstrap_configuration: Some(bootstrap.into()),
                                provisioning: None,
                            },
                        ),
                    ),
                }))
                .await?;
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        })
        .await;
        let initialized: TestResult = match initialized {
            Ok(result) => result,
            Err(error) => Err(error.into()),
        };
        if let Err(error) = initialized {
            startup.abort();
            let _ = startup.await;
            return Err(error);
        }
        let mut running = match tokio::time::timeout(RPC_BOUND, &mut startup).await {
            Ok(result) => result??,
            Err(error) => {
                startup.abort();
                let _ = startup.await;
                return Err(error.into());
            }
        };
        let report = match report_at(&control_address, &resource, &identity).await {
            Ok(report) => report,
            Err(error) => {
                running.shutdown();
                let _ = tokio::time::timeout(RPC_BOUND, running.wait()).await;
                let _ = tokio::time::timeout(RPC_BOUND, service.shutdown_and_drain()).await;
                return Err(error);
            }
        };
        assert_eq!(report.replica, identity);
        Ok(Self {
            running,
            service,
            identity,
            storage: report.storage_incarnation,
            history: report.history,
            revision: 0,
            control_address,
            session: report.process_session_id,
        })
    }

    async fn report(&self) -> TestResult<PublicV1AgentReport> {
        report_at(
            &self.control_address,
            &self.history.resource_uid,
            &self.identity,
        )
        .await
    }

    async fn command(
        &mut self,
        program: PublicOperationProgram,
    ) -> TestResult<(PublicV1OperationCommand, PublicV1AgentReport)> {
        let (id, class) = match &program {
            PublicOperationProgram::Build(build) => (
                build.attempt.clone(),
                PublicOperationClass::Build {
                    target: build.replica.replica_id,
                },
            ),
            _ => {
                self.revision += 1;
                (
                    OperationId::new(format!(
                        "{}-{}-{}",
                        self.identity.replica_id, self.session, self.revision
                    )),
                    if matches!(program, PublicOperationProgram::Close) {
                        PublicOperationClass::Close
                    } else {
                        PublicOperationClass::Authority
                    },
                )
            }
        };
        let operation = PublicV1OperationCommand {
            target: self.identity.clone(),
            intent: PublicOperationIntent {
                identity: PublicV1Identity::new(1),
                operation_id: id,
                revision: self.revision,
                process_session_id: self.session.clone(),
                class,
                input_digest: program.digest(),
                replay_disposition: program.replay_disposition(),
                lifecycle: None,
                program: Some(program),
            },
        };
        let report = tokio::time::timeout(RPC_BOUND, async {
            let mut client = proto::agent_control_client::AgentControlClient::connect(
                self.control_address.clone(),
            )
            .await?;
            let response = client
                .execute(authorized(proto::ExecuteCommandRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: self.history.resource_uid.to_string(),
                    target: Some(self.identity.clone().into()),
                    expected_process_session_id: self.session.to_string(),
                    command: Some(proto::execute_command_request::Command::PublicV1Operation(
                        convert::public_v1_operation_command_to_proto(&operation)?,
                    )),
                }))
                .await?
                .into_inner();
            Ok::<_, Box<dyn std::error::Error + Send + Sync>>(
                convert::normalize_public_v1_agent_status_report(
                    response
                        .observation
                        .ok_or("operation response omitted observation")?,
                )?,
            )
        })
        .await??;
        let completion = report
            .completed_operations
            .iter()
            .find(|completion| {
                completion.operation_id == operation.intent.operation_id
                    && completion.revision == operation.intent.revision
                    && completion.process_session_id == self.session
                    && completion.input_digest == operation.intent.input_digest
            })
            .ok_or("operation response omitted exact completion")?;
        if completion.result != PublicV1OperationResult::Succeeded {
            return Err(format!("public operation failed: {completion:?}").into());
        }
        Ok((operation, report))
    }

    async fn open(&mut self, role: ReplicaRole, epoch: Epoch) -> TestResult {
        self.command(PublicOperationProgram::Open {
            replica: self.identity.clone(),
            existing: false,
        })
        .await?;
        self.command(PublicOperationProgram::Role { epoch, role })
            .await?;
        if role == ReplicaRole::Primary {
            self.command(PublicOperationProgram::Epoch { epoch })
                .await?;
        }
        Ok(())
    }

    async fn install_authority(&self, authority: PublicInboundBuildAuthority) -> TestResult {
        tokio::time::timeout(RPC_BOUND, async {
            let mut client = proto::replica_peer_client::ReplicaPeerClient::connect(
                self.control_address.clone(),
            )
            .await?;
            client
                .execute(authorized(proto::ExecuteCommandRequest {
                    protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                    resource_uid: self.history.resource_uid.to_string(),
                    target: Some(self.identity.clone().into()),
                    expected_process_session_id: self.session.to_string(),
                    command: Some(proto::execute_command_request::Command::PublicV1History(
                        convert::public_v1_history_command_to_proto(
                            &PublicV1HistoryCommand::InstallInboundBuildAuthority {
                                authority: Box::new(authority),
                            },
                        )?,
                    )),
                }))
                .await?;
            Ok(())
        })
        .await?
    }

    async fn close(mut self) -> TestResult {
        let closed = self.command(PublicOperationProgram::Close).await;
        self.running.shutdown();
        let waited = tokio::time::timeout(RPC_BOUND, self.running.wait()).await;
        let drained = tokio::time::timeout(RPC_BOUND, self.service.shutdown_and_drain()).await;
        let errors: Vec<_> = [
            closed
                .map(|_| ())
                .map_err(|error| format!("public Close: {error}")),
            waited
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()))
                .map_err(|error| format!("RunningReplica::wait: {error}")),
            drained
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()))
                .map_err(|error| format!("shutdown_and_drain: {error}")),
        ]
        .into_iter()
        .filter_map(Result::err)
        .collect();
        if !errors.is_empty() {
            return Err(errors.join("; ").into());
        }
        Ok(())
    }
}

#[tokio::test]
async fn public_build_consumes_target_context_and_installs_nonempty_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let source_root = root.path().join("source");
    let target_root = root.path().join("target");
    let target_application = target_root.join("application");
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
        &source_root,
        1,
        1,
        source_context.clone(),
        target_context.clone(),
    )
    .await
    .unwrap();
    let mut target = match Node::start(&target_root, 2, 2, target_context.clone(), Vec::new()).await
    {
        Ok(target) => target,
        Err(error) => {
            let cleanup = source.close().await;
            panic!("target startup failed: {error}; source cleanup: {cleanup:?}");
        }
    };
    let mut copied_snapshot = None;
    let outcome =
        std::panic::AssertUnwindSafe(tokio::time::timeout(Duration::from_secs(90), async {
            let epoch = Epoch::new(0, 1);
            source.open(ReplicaRole::Primary, epoch).await?;
            let current = ConfigurationDescriptor::new(
                epoch,
                source.identity.replica_id,
                vec![ConfigurationMember {
                    identity: source.identity.clone(),
                    role: ReplicaRole::Primary,
                }],
                1,
            );
            source
                .command(PublicOperationProgram::Configuration(PublicConfiguration {
                    current: current.clone(),
                    previous: None,
                    replicas: Vec::new(),
                }))
                .await?;
            let mut committed = 0;
            for value in &expected_values {
                let write = source.service.begin_write(value.clone()).await?;
                let assigned = write.lsn;
                committed = write.wait().await?;
                assert_eq!(committed, assigned);
            }
            assert!(committed > 0);
            assert_eq!(source.service.values().await?, expected_values);
            target.open(ReplicaRole::IdleSecondary, epoch).await?;
            assert_eq!(
                ExternalService::read_persisted_snapshot(&target_application)?,
                PersistedSnapshot {
                    committed_lsn: 0,
                    values: Vec::new()
                },
            );
            assert_eq!(
                target.service.copy_context_observations(),
                CopyContextObservations::default()
            );
            let source_report = source.report().await?;
            let target_report = target.report().await?;
            assert_eq!(source_report.history, target_report.history);
            assert_eq!(source_report.progress.current(), Some(committed));
            assert_eq!(target_report.role, ReplicaRole::IdleSecondary);
            let source_address = source_report
                .replication_address
                .ok_or("source endpoint missing")?;
            let target_address = target_report
                .replication_address
                .ok_or("target endpoint missing")?;
            let attempt = OperationId::new(format!("target-context-build-{}", target.session));
            let build = PublicBuildInput {
                attempt: attempt.clone(),
                epoch,
                current_configuration_id: Some(current.configuration_id.clone()),
                replica: target.identity.clone(),
                target_storage: target.storage.clone(),
                process_session_id: target.session.clone(),
                source_address: source_address.clone(),
                replication_address: target_address.clone(),
                history: Some(source.history.clone()),
            };
            let authority = PublicInboundBuildAuthority {
                operation_id: attempt.clone(),
                operation_revision: source.revision,
                attempt: PublicBuildAttemptReference {
                    resource_uid: source.history.resource_uid.clone(),
                    epoch,
                    build_id: attempt.clone(),
                    source: source.identity.clone(),
                    source_storage: source.storage.clone(),
                    source_session: source.session.clone(),
                    target: target.identity.clone(),
                    target_storage: target.storage.clone(),
                    target_session: target.session.clone(),
                },
                history: source.history.clone(),
                current_configuration_id: current.configuration_id.clone(),
                source_address,
                target_address,
            };
            target.install_authority(authority.clone()).await?;
            let (operation, report) = source
                .command(PublicOperationProgram::Build(build.clone()))
                .await?;
            let completions: Vec<_> = report
                .completed_operations
                .iter()
                .filter(|completion| completion.operation_id == attempt)
                .collect();
            assert_eq!(
                completions,
                [&PublicV1OperationCompletion {
                    operation_id: authority.operation_id.clone(),
                    revision: authority.operation_revision,
                    call_kind: PublicV1CallKind::Build,
                    process_session_id: authority.attempt.source_session.clone(),
                    input_digest: operation.intent.input_digest.clone(),
                    replay_disposition: operation.intent.replay_disposition,
                    history: authority.history.clone(),
                    storage_incarnation: authority.attempt.source_storage.clone(),
                    previous_configuration_id: None,
                    current_configuration_id: Some(authority.current_configuration_id.clone()),
                    catch_up_mode: None,
                    catch_up_specific_quorum: None,
                    result: PublicV1OperationResult::Succeeded,
                }]
            );
            assert_eq!(
                report.remote_builds,
                [PublicV1RemoteBuildObservation {
                    operation_id: attempt,
                    revision: authority.operation_revision,
                    build,
                    state: PublicV1RemoteBuildState::Ready,
                }]
            );
            println!("observed Build completions: {completions:?}");
            println!("observed remote Builds: {:?}", report.remote_builds);
            assert_eq!(
                report.configuration.as_ref().unwrap().voting.current,
                current
            );
            assert!(
                current
                    .members
                    .iter()
                    .all(|member| member.identity != target.identity)
            );
            assert_eq!(
                source.service.copy_context_observations(),
                CopyContextObservations {
                    context_acquisitions: 0,
                    copy_state_acquisitions: vec![CopyStateObservation {
                        up_to: committed,
                        items: target_context.clone(),
                        reached_eof: true,
                    }],
                }
            );
            assert_ne!(target_context, source_context);
            assert_eq!(
                target.service.copy_context_observations(),
                CopyContextObservations {
                    context_acquisitions: 1,
                    copy_state_acquisitions: Vec::new(),
                }
            );
            println!(
                "observed source context: {:?}; target context: {:?}",
                source.service.copy_context_observations(),
                target.service.copy_context_observations()
            );
            let snapshot = ExternalService::read_persisted_snapshot(&target_application)?;
            assert_eq!(
                snapshot,
                PersistedSnapshot {
                    committed_lsn: committed,
                    values: expected_values.clone(),
                }
            );
            assert_ne!(snapshot.values, target_context);
            println!("checked target snapshot before shutdown: {snapshot:?}");
            let idle = target.report().await?;
            assert_eq!(idle.role, ReplicaRole::IdleSecondary);
            assert_ne!(idle.read_status, AccessStatus::Granted);
            assert!(matches!(
                target.service.values().await,
                Err(kuberic_runtime::RuntimeError::ReadClosed(_))
            ));
            copied_snapshot = Some(snapshot);
            Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
        }))
        .catch_unwind()
        .await;
    let source_closed = source.close().await;
    let target_closed = target.close().await;
    println!(
        "source Close/wait/drain: {source_closed:?}; target Close/wait/drain: {target_closed:?}"
    );
    assert!(
        source_closed.is_ok(),
        "source Close/wait/drain failed: {source_closed:?}"
    );
    assert!(
        target_closed.is_ok(),
        "target Close/wait/drain failed: {target_closed:?}"
    );
    match outcome {
        Ok(result) => result
            .expect("bounded public copy-context journey")
            .unwrap(),
        Err(panic) => std::panic::resume_unwind(panic),
    }
    let persisted = ExternalService::read_persisted_snapshot(&target_application).unwrap();
    println!("checked target snapshot after shutdown: {persisted:?}");
    assert_eq!(
        persisted,
        copied_snapshot.expect("successful Build observed the checked target snapshot"),
    );
}
