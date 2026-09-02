use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot, watch};
use tonic::transport::Server;
use tracing::{info, warn};

use crate::error::{KubericError, Result};
use crate::events::{LifecycleEvent, ReplicatorControlEvent};
use crate::handles::PartitionState;
use crate::replicator::{OpenContext, ReplicatorHandle};
use crate::types::{
    AccessStatus, CancellationToken, DataLossAction, Epoch, Lsn, OpenMode,
    ReplicaConfigurationMode, ReplicaConfigurationStatus, ReplicaDeactivationInfo,
    ReplicaElectionConfiguration, ReplicaId, ReplicaInfo, ReplicaInstanceId, ReplicaSetConfig,
    ReplicaSetQuorumMode, Role,
};

const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const BUILD_REPLY_TIMEOUT: Duration = Duration::from_secs(600);
const DATA_LOSS_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
// ---------------------------------------------------------------------------
// RuntimeEffectCommand — what the replica agent sends to the runtime
// ---------------------------------------------------------------------------

/// One ordered service/replicator effect delegated by [`ReplicaAgent`].
pub enum RuntimeEffect {
    Open {
        mode: OpenMode,
    },
    Close,
    ChangeRole {
        epoch: Epoch,
        role: Role,
    },
    UpdateEpoch {
        epoch: Epoch,
    },
    UpdateCatchUpConfiguration {
        current: ReplicaSetConfig,
        previous: ReplicaSetConfig,
    },
    UpdateTrackedCatchUpConfiguration {
        current: ReplicaSetConfig,
        previous: ReplicaSetConfig,
        required_build_key: String,
    },
    UpdateCurrentConfiguration {
        current: ReplicaSetConfig,
    },
    WaitForCatchUpQuorum {
        mode: ReplicaSetQuorumMode,
    },
    BuildReplica {
        replica: ReplicaInfo,
    },
    StartTrackedBuild {
        execution_id: String,
        build_key: String,
        target_agent_generation: crate::types::AgentGeneration,
        replica: ReplicaInfo,
    },
    StartTrackedCatchUpQuorum {
        execution_id: String,
        mode: ReplicaSetQuorumMode,
    },
    CancelTrackedOperation {
        execution_id: String,
    },
    RemoveReplica {
        replica_id: ReplicaId,
        instance_id: ReplicaInstanceId,
    },
    OnDataLoss {
        expected_epoch: Epoch,
    },
    RevokeWriteStatus,
    RecordElectionConfiguration {
        configuration: ReplicaElectionConfiguration,
    },
}

#[derive(Debug)]
pub enum RuntimeEffectResult {
    Unit,
    DataLoss(DataLossAction),
}

pub struct RuntimeEffectCommand {
    pub effect: RuntimeEffect,
    pub reply: oneshot::Sender<Result<RuntimeEffectResult>>,
}

/// Non-blocking runtime state published to the replica agent.
#[derive(Clone)]
pub struct RuntimeControlSnapshot {
    pub instance_id: ReplicaInstanceId,
    pub role: Role,
    pub epoch: Epoch,
    pub configuration: Option<ReplicaConfigurationStatus>,
    pub election_configuration: Option<ReplicaElectionConfiguration>,
    pub deactivation_info: Option<ReplicaDeactivationInfo>,
    pub build_observation: Option<crate::add_replica::RuntimeBuildObservation>,
    pub quorum_wait_observation: Option<crate::add_replica::RuntimeQuorumWaitObservation>,
    pub partition_state: Option<Arc<PartitionState>>,
}

enum TrackedRuntimeKind {
    Build { cancellation: CancellationToken },
    CatchUpQuorum,
}

struct ActiveTrackedRuntime {
    execution_id: String,
    kind: TrackedRuntimeKind,
}

enum TrackedRuntimeResult {
    Build(Result<Lsn>),
    CatchUpQuorum(Result<()>),
}

struct TrackedRuntimeCompletion {
    execution_id: String,
    result: TrackedRuntimeResult,
}

// ---------------------------------------------------------------------------
// PodRuntime
// ---------------------------------------------------------------------------

pub struct PodRuntime {
    lifecycle_tx: mpsc::Sender<LifecycleEvent>,
    cmd_rx: mpsc::Receiver<RuntimeEffectCommand>,
    replicator_handle: Option<ReplicatorHandle>,
    shutdown: CancellationToken,
    reply_timeout: Duration,
    role: Role,
    epoch: Epoch,
    replica_id: ReplicaId,
    instance_id: ReplicaInstanceId,
    data_bind: String,
    configuration: Option<ReplicaConfigurationStatus>,
    election_configuration: Option<ReplicaElectionConfiguration>,
    deactivation_info: Option<ReplicaDeactivationInfo>,
    build_observation: Option<crate::add_replica::RuntimeBuildObservation>,
    quorum_wait_observation: Option<crate::add_replica::RuntimeQuorumWaitObservation>,
    active_tracked: Option<ActiveTrackedRuntime>,
    tracked_completion_tx: mpsc::UnboundedSender<TrackedRuntimeCompletion>,
    tracked_completion_rx: mpsc::UnboundedReceiver<TrackedRuntimeCompletion>,
    fault_tx: mpsc::Sender<crate::types::FaultType>,
    status_tx: watch::Sender<RuntimeControlSnapshot>,
}

pub struct PodRuntimeBundle {
    pub runtime: PodRuntime,
    pub lifecycle_rx: mpsc::Receiver<LifecycleEvent>,
    pub control_address: String,
}

pub struct PodRuntimeBuilder {
    replica_id: ReplicaId,
    instance_id: ReplicaInstanceId,
    reply_timeout: Duration,
    control_bind: String,
    data_bind: String,
    removal_clock: Arc<dyn crate::remove_replica::RemoveReplicaClock>,
}

impl PodRuntimeBuilder {
    pub fn new(replica_id: ReplicaId) -> Self {
        let generation: u128 = rand::random();
        Self {
            replica_id,
            instance_id: ReplicaInstanceId::new(format!("local-{replica_id}-{generation:032x}")),
            reply_timeout: DEFAULT_REPLY_TIMEOUT,
            control_bind: "127.0.0.1:0".to_string(),
            data_bind: "127.0.0.1:0".to_string(),
            removal_clock: Arc::new(crate::remove_replica::SystemRemoveReplicaClock),
        }
    }

    pub fn instance_id(mut self, instance_id: ReplicaInstanceId) -> Self {
        self.instance_id = instance_id;
        self
    }

    pub fn reply_timeout(mut self, timeout: Duration) -> Self {
        self.reply_timeout = timeout;
        self
    }

    pub fn control_bind(mut self, addr: String) -> Self {
        self.control_bind = addr;
        self
    }

    pub fn data_bind(mut self, addr: String) -> Self {
        self.data_bind = addr;
        self
    }

    pub fn removal_clock(
        mut self,
        clock: Arc<dyn crate::remove_replica::RemoveReplicaClock>,
    ) -> Self {
        self.removal_clock = clock;
        self
    }

    pub async fn build(self) -> Result<PodRuntimeBundle> {
        let shutdown = CancellationToken::new();
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel(16);
        let (agent_tx, agent_rx) = mpsc::channel(16);
        let (runtime_tx, cmd_rx) = mpsc::channel(16);
        let (fault_tx, fault_rx) = mpsc::channel(16);
        let initial_status = RuntimeControlSnapshot {
            instance_id: self.instance_id.clone(),
            role: Role::Unknown,
            epoch: Epoch::default(),
            configuration: None,
            election_configuration: None,
            deactivation_info: None,
            build_observation: None,
            quorum_wait_observation: None,
            partition_state: None,
        };
        let (status_tx, status_rx) = watch::channel(initial_status);
        let (tracked_completion_tx, tracked_completion_rx) = mpsc::unbounded_channel();

        // Control plane gRPC server routes through the pod-local agent.
        // PartitionState is not available yet — it's created by the replicator
        // at Open time. ControlServer needs to work without it initially.
        let control_server = crate::grpc::server::ControlServer::new(agent_tx.clone());
        let peer_server = crate::grpc::peer_server::PeerServer::new(agent_tx);
        let replica_agent = crate::replica_agent::ReplicaAgent::new_with_clock(
            self.replica_id,
            self.instance_id.clone(),
            agent_rx,
            runtime_tx,
            status_rx,
            fault_rx,
            shutdown.child_token(),
            self.removal_clock,
        );
        tokio::spawn(replica_agent.serve());
        let control_listener = tokio::net::TcpListener::bind(&self.control_bind)
            .await
            .map_err(|e| KubericError::Internal(Box::new(e)))?;
        let control_addr = control_listener.local_addr().unwrap();
        let control_address = format!("http://{}", control_addr);

        let ctrl_shutdown = shutdown.child_token();
        tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(
                    crate::proto::replicator_control_server::ReplicatorControlServer::new(
                        control_server,
                    ),
                )
                .add_service(
                    crate::proto::replica_lifecycle_peer_server::ReplicaLifecyclePeerServer::new(
                        peer_server,
                    ),
                )
                .serve_with_incoming_shutdown(
                    tokio_stream::wrappers::TcpListenerStream::new(control_listener),
                    ctrl_shutdown.cancelled(),
                )
                .await;
        });

        tokio::time::sleep(Duration::from_millis(50)).await;

        info!(
            replica_id = self.replica_id,
            %control_address,
            "pod runtime started (replicator deferred to Open)"
        );

        let runtime = PodRuntime {
            lifecycle_tx,
            cmd_rx,
            replicator_handle: None,
            shutdown,
            reply_timeout: self.reply_timeout,
            role: Role::Unknown,
            epoch: Epoch::default(),
            replica_id: self.replica_id,
            instance_id: self.instance_id,
            data_bind: self.data_bind,
            configuration: None,
            election_configuration: None,
            deactivation_info: None,
            build_observation: None,
            quorum_wait_observation: None,
            active_tracked: None,
            tracked_completion_tx,
            tracked_completion_rx,
            fault_tx,
            status_tx,
        };

        Ok(PodRuntimeBundle {
            runtime,
            lifecycle_rx,
            control_address,
        })
    }
}

impl PodRuntime {
    pub fn builder(replica_id: ReplicaId) -> PodRuntimeBuilder {
        PodRuntimeBuilder::new(replica_id)
    }

    /// Get the shutdown token. Cancelling it triggers graceful shutdown.
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// Run the runtime command loop. Processes operator commands from the
    /// gRPC control server with correct replicator/user event ordering.
    /// Blocks until shutdown.
    pub async fn serve(mut self) {
        info!("PodRuntime serve loop started");
        loop {
            tokio::select! {
                biased;
                completion = self.tracked_completion_rx.recv() => {
                    let Some(completion) = completion else {
                        break;
                    };
                    self.handle_tracked_completion(completion);
                    self.publish_status();
                }
                command = self.cmd_rx.recv() => {
                    let Some(command) = command else {
                        break;
                    };
                    let result = match command.effect {
                        RuntimeEffect::StartTrackedBuild {
                            execution_id,
                            build_key,
                            target_agent_generation,
                            replica,
                        } => self.start_tracked_build(
                            execution_id,
                            build_key,
                            target_agent_generation,
                            replica,
                        ),
                        RuntimeEffect::StartTrackedCatchUpQuorum { execution_id, mode } => {
                            self.start_tracked_catch_up_quorum(execution_id, mode)
                        }
                        RuntimeEffect::CancelTrackedOperation { execution_id } => {
                            self.cancel_tracked_operation(&execution_id).await
                        }
                        effect => self.execute_effect(effect).await,
                    };
                    self.publish_status();
                    let _ = command.reply.send(result);
                }
            }
        }
        self.shutdown.cancel();
    }

    async fn execute_effect(&mut self, effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        let result = match effect {
            RuntimeEffect::Open { mode } => self.handle_open(mode).await,
            RuntimeEffect::Close => self.handle_close().await,
            RuntimeEffect::ChangeRole { epoch, role } => {
                self.require_handle()?;
                self.handle_change_role(epoch, role).await
            }
            RuntimeEffect::UpdateEpoch { epoch } => self.handle_update_epoch(epoch).await,
            RuntimeEffect::UpdateCatchUpConfiguration { current, previous } => {
                let observed_configuration = ReplicaConfigurationStatus::from_config(
                    ReplicaConfigurationMode::CatchUp,
                    &current,
                );
                let result = self
                    .send_replicator_control(|reply| {
                        ReplicatorControlEvent::UpdateCatchUpConfiguration {
                            current,
                            previous,
                            required_build_key: None,
                            reply,
                        }
                    })
                    .await;
                if result.is_ok() && self.role == Role::Primary {
                    self.require_handle()?
                        .state()
                        .set_write_status(AccessStatus::Granted);
                }
                if result.is_ok() {
                    self.configuration = Some(observed_configuration);
                }
                result
            }
            RuntimeEffect::UpdateTrackedCatchUpConfiguration {
                current,
                previous,
                required_build_key,
            } => {
                let observed_configuration = ReplicaConfigurationStatus::from_config(
                    ReplicaConfigurationMode::CatchUp,
                    &current,
                );
                let result = self
                    .send_replicator_control(|reply| {
                        ReplicatorControlEvent::UpdateCatchUpConfiguration {
                            current,
                            previous,
                            required_build_key: Some(required_build_key),
                            reply,
                        }
                    })
                    .await;
                if result.is_ok() && self.role == Role::Primary {
                    self.require_handle()?
                        .state()
                        .set_write_status(AccessStatus::Granted);
                }
                if result.is_ok() {
                    self.configuration = Some(observed_configuration);
                }
                result
            }
            RuntimeEffect::UpdateCurrentConfiguration { current } => {
                let observed_configuration = ReplicaConfigurationStatus::from_config(
                    ReplicaConfigurationMode::Current,
                    &current,
                );
                let result = self
                    .send_replicator_control(|reply| {
                        ReplicatorControlEvent::UpdateCurrentConfiguration { current, reply }
                    })
                    .await;
                if result.is_ok() && self.role == Role::Primary {
                    self.require_handle()?
                        .state()
                        .set_write_status(AccessStatus::Granted);
                }
                if result.is_ok() {
                    self.configuration = Some(observed_configuration);
                }
                result
            }
            RuntimeEffect::WaitForCatchUpQuorum { mode } => {
                self.send_replicator_control(|reply| ReplicatorControlEvent::WaitForCatchUpQuorum {
                    wait_id: None,
                    mode,
                    reply,
                })
                .await
            }
            RuntimeEffect::BuildReplica { replica } => self
                .send_replicator_control_with_timeout(
                    |reply| ReplicatorControlEvent::BuildReplica {
                        replica,
                        build_key: None,
                        cancellation: CancellationToken::new(),
                        reply,
                    },
                    BUILD_REPLY_TIMEOUT,
                )
                .await
                .map(|_| ()),
            RuntimeEffect::RemoveReplica {
                replica_id,
                instance_id,
            } => {
                self.send_replicator_control(|reply| ReplicatorControlEvent::RemoveReplica {
                    replica_id,
                    instance_id,
                    reply,
                })
                .await
            }
            RuntimeEffect::OnDataLoss { expected_epoch } => {
                return self
                    .handle_on_data_loss(Some(expected_epoch))
                    .await
                    .map(RuntimeEffectResult::DataLoss);
            }
            RuntimeEffect::RevokeWriteStatus => {
                info!("revoking write status for switchover");
                self.require_handle()?
                    .state()
                    .set_write_status(AccessStatus::ReconfigurationPending);
                Ok(())
            }
            RuntimeEffect::RecordElectionConfiguration { configuration } => {
                self.election_configuration = Some(configuration);
                Ok(())
            }
            RuntimeEffect::StartTrackedBuild { .. }
            | RuntimeEffect::StartTrackedCatchUpQuorum { .. }
            | RuntimeEffect::CancelTrackedOperation { .. } => {
                unreachable!("tracked runtime effects are handled by the serve loop")
            }
        };
        result.map(|()| RuntimeEffectResult::Unit)
    }

    fn start_tracked_build(
        &mut self,
        execution_id: String,
        build_key: String,
        target_agent_generation: crate::types::AgentGeneration,
        replica: ReplicaInfo,
    ) -> Result<RuntimeEffectResult> {
        if execution_id.is_empty() || build_key.is_empty() {
            return Err(KubericError::Internal(
                "tracked build identity must not be empty".into(),
            ));
        }
        if let Some(observation) = &self.build_observation {
            let exact = observation.build_key == build_key
                && observation.target_replica_id == replica.id
                && observation.target_instance_id == replica.instance_id
                && observation.target_agent_generation == target_agent_generation;
            if exact {
                return match observation.state {
                    crate::add_replica::RuntimeBuildState::InProgress
                    | crate::add_replica::RuntimeBuildState::Completed => {
                        Ok(RuntimeEffectResult::Unit)
                    }
                    crate::add_replica::RuntimeBuildState::Failed
                    | crate::add_replica::RuntimeBuildState::Cancelled => {
                        Err(KubericError::Internal(
                            observation
                                .error
                                .clone()
                                .unwrap_or_else(|| "tracked build did not complete".to_string())
                                .into(),
                        ))
                    }
                };
            }
            if observation.execution_id == execution_id {
                return Err(KubericError::ActionIdConflict {
                    action_id: execution_id,
                });
            }
        }
        if self.active_tracked.is_some() {
            return Err(KubericError::AgentBusy);
        }
        let handle = self.require_handle()?.clone();
        let cancellation = CancellationToken::new();
        self.build_observation = Some(crate::add_replica::RuntimeBuildObservation {
            execution_id: execution_id.clone(),
            build_key: build_key.clone(),
            target_replica_id: replica.id,
            target_instance_id: replica.instance_id.clone(),
            target_agent_generation,
            state: crate::add_replica::RuntimeBuildState::InProgress,
            copy_lsn: None,
            error: None,
        });
        self.active_tracked = Some(ActiveTrackedRuntime {
            execution_id: execution_id.clone(),
            kind: TrackedRuntimeKind::Build {
                cancellation: cancellation.clone(),
            },
        });
        let completion_tx = self.tracked_completion_tx.clone();
        tokio::spawn(async move {
            let result = handle
                .send_control(
                    |reply| ReplicatorControlEvent::BuildReplica {
                        replica,
                        build_key: Some(build_key),
                        cancellation,
                        reply,
                    },
                    BUILD_REPLY_TIMEOUT,
                )
                .await;
            let _ = completion_tx.send(TrackedRuntimeCompletion {
                execution_id,
                result: TrackedRuntimeResult::Build(result),
            });
        });
        Ok(RuntimeEffectResult::Unit)
    }

    fn start_tracked_catch_up_quorum(
        &mut self,
        execution_id: String,
        mode: ReplicaSetQuorumMode,
    ) -> Result<RuntimeEffectResult> {
        if execution_id.is_empty() {
            return Err(KubericError::Internal(
                "tracked quorum wait identity must not be empty".into(),
            ));
        }
        if let Some(observation) = &self.quorum_wait_observation
            && observation.execution_id == execution_id
        {
            return match observation.state {
                crate::add_replica::RuntimeBuildState::InProgress
                | crate::add_replica::RuntimeBuildState::Completed => Ok(RuntimeEffectResult::Unit),
                crate::add_replica::RuntimeBuildState::Failed
                | crate::add_replica::RuntimeBuildState::Cancelled => Err(KubericError::Internal(
                    observation
                        .error
                        .clone()
                        .unwrap_or_else(|| "tracked quorum wait did not complete".to_string())
                        .into(),
                )),
            };
        }
        if self.active_tracked.is_some() {
            return Err(KubericError::AgentBusy);
        }
        let handle = self.require_handle()?.clone();
        self.quorum_wait_observation = Some(crate::add_replica::RuntimeQuorumWaitObservation {
            execution_id: execution_id.clone(),
            state: crate::add_replica::RuntimeBuildState::InProgress,
            error: None,
        });
        self.active_tracked = Some(ActiveTrackedRuntime {
            execution_id: execution_id.clone(),
            kind: TrackedRuntimeKind::CatchUpQuorum,
        });
        let completion_tx = self.tracked_completion_tx.clone();
        tokio::spawn(async move {
            let wait_id = execution_id.clone();
            let result = handle
                .send_control(
                    |reply| ReplicatorControlEvent::WaitForCatchUpQuorum {
                        wait_id: Some(wait_id),
                        mode,
                        reply,
                    },
                    BUILD_REPLY_TIMEOUT,
                )
                .await;
            let _ = completion_tx.send(TrackedRuntimeCompletion {
                execution_id,
                result: TrackedRuntimeResult::CatchUpQuorum(result),
            });
        });
        Ok(RuntimeEffectResult::Unit)
    }

    async fn cancel_tracked_operation(
        &mut self,
        execution_id: &str,
    ) -> Result<RuntimeEffectResult> {
        let Some(active) = &self.active_tracked else {
            return Ok(RuntimeEffectResult::Unit);
        };
        if active.execution_id != execution_id {
            return Err(KubericError::AgentBusy);
        }
        match &active.kind {
            TrackedRuntimeKind::Build { cancellation } => cancellation.cancel(),
            TrackedRuntimeKind::CatchUpQuorum => {
                self.send_replicator_control(|reply| {
                    ReplicatorControlEvent::CancelCatchUpQuorumWait {
                        wait_id: execution_id.to_string(),
                        reply,
                    }
                })
                .await?;
            }
        }
        Ok(RuntimeEffectResult::Unit)
    }

    fn handle_tracked_completion(&mut self, completion: TrackedRuntimeCompletion) {
        let Some(active) = self.active_tracked.take() else {
            warn!(
                execution_id = completion.execution_id,
                "discarding tracked runtime completion without active operation"
            );
            return;
        };
        if active.execution_id != completion.execution_id {
            warn!(
                expected = active.execution_id,
                actual = completion.execution_id,
                "discarding stale tracked runtime completion"
            );
            self.active_tracked = Some(active);
            return;
        }
        match completion.result {
            TrackedRuntimeResult::Build(result) => {
                let Some(observation) = self.build_observation.as_mut() else {
                    return;
                };
                match result {
                    Ok(copy_lsn) => {
                        observation.state = crate::add_replica::RuntimeBuildState::Completed;
                        observation.copy_lsn = Some(copy_lsn);
                        observation.error = None;
                    }
                    Err(error) => {
                        observation.state = if matches!(error, KubericError::Cancelled) {
                            crate::add_replica::RuntimeBuildState::Cancelled
                        } else {
                            crate::add_replica::RuntimeBuildState::Failed
                        };
                        observation.error =
                            Some(crate::add_replica::normalize_add_error(&error.to_string()));
                    }
                }
            }
            TrackedRuntimeResult::CatchUpQuorum(result) => {
                let Some(observation) = self.quorum_wait_observation.as_mut() else {
                    return;
                };
                match result {
                    Ok(()) => {
                        observation.state = crate::add_replica::RuntimeBuildState::Completed;
                        observation.error = None;
                    }
                    Err(error) => {
                        observation.state = if matches!(error, KubericError::Cancelled) {
                            crate::add_replica::RuntimeBuildState::Cancelled
                        } else {
                            crate::add_replica::RuntimeBuildState::Failed
                        };
                        observation.error =
                            Some(crate::add_replica::normalize_add_error(&error.to_string()));
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Command handlers with correct ordering
    // -----------------------------------------------------------------------

    /// Get handle or return error for pre-Open commands.
    fn require_handle(&self) -> Result<&ReplicatorHandle> {
        self.replicator_handle
            .as_ref()
            .ok_or(KubericError::Internal("replicator not opened".into()))
    }

    async fn handle_open(&mut self, mode: OpenMode) -> Result<()> {
        if self.replicator_handle.is_some() {
            return Err(KubericError::Internal("already opened".into()));
        }
        self.reset_lifecycle_evidence_for_open();

        // 1. Send OpenContext to user, receive ReplicatorHandle back.
        let handle: ReplicatorHandle = self
            .send_lifecycle(|reply| LifecycleEvent::Open {
                ctx: OpenContext {
                    replica_id: self.replica_id,
                    open_mode: mode,
                    data_bind: self.data_bind.clone(),
                    token: self.shutdown.child_token(),
                    fault_tx: self.fault_tx.clone(),
                },
                reply,
            })
            .await?;

        // 2. Open the replicator (via channel)
        handle
            .send_control(
                |r| ReplicatorControlEvent::Open { mode, reply: r },
                self.reply_timeout,
            )
            .await?;

        info!(
            data_address = %handle.data_address(),
            "replicator opened"
        );

        // 3. Store handle for future lifecycle calls
        self.replicator_handle = Some(handle);
        Ok(())
    }

    fn reset_lifecycle_evidence_for_open(&mut self) {
        self.role = Role::Unknown;
        self.epoch = Epoch::default();
        self.configuration = None;
        self.election_configuration = None;
        self.deactivation_info = None;
    }

    async fn handle_change_role(&mut self, epoch: Epoch, new_role: Role) -> Result<()> {
        let old_role = self.role;
        let is_promotion = new_role == Role::Primary
            || (new_role == Role::ActiveSecondary && old_role == Role::IdleSecondary);

        let handle = self.require_handle()?;

        if is_promotion {
            // Promotion: replicator first, then status, then user
            handle
                .send_control(
                    |reply| ReplicatorControlEvent::ChangeRole {
                        epoch,
                        role: new_role,
                        reply,
                    },
                    self.reply_timeout,
                )
                .await?;
            self.set_status_for_role(new_role);
            let _: String = self
                .send_lifecycle(|reply| LifecycleEvent::ChangeRole { new_role, reply })
                .await?;
        } else {
            // Demotion: status first, then user, then replicator
            self.set_status_for_role(new_role);
            let _: String = self
                .send_lifecycle(|reply| LifecycleEvent::ChangeRole { new_role, reply })
                .await?;
            handle
                .send_control(
                    |reply| ReplicatorControlEvent::ChangeRole {
                        epoch,
                        role: new_role,
                        reply,
                    },
                    self.reply_timeout,
                )
                .await?;
        }

        self.role = new_role;
        self.epoch = epoch;
        if matches!(old_role, Role::Primary | Role::ActiveSecondary)
            || matches!(new_role, Role::Primary | Role::ActiveSecondary)
        {
            let catch_up_lsn = self
                .replicator_handle
                .as_ref()
                .map_or(0, |handle| handle.state().committed_lsn());
            self.deactivation_info = Some(ReplicaDeactivationInfo {
                epoch,
                catch_up_lsn,
            });
        }
        Ok(())
    }

    async fn handle_close(&mut self) -> Result<()> {
        if matches!(self.role, Role::Primary | Role::ActiveSecondary)
            && let Some(handle) = &self.replicator_handle
        {
            self.deactivation_info = Some(ReplicaDeactivationInfo {
                epoch: self.epoch,
                catch_up_lsn: handle.state().committed_lsn(),
            });
        }
        if let Some(handle) = &self.replicator_handle {
            handle
                .state()
                .set_read_status(AccessStatus::ReconfigurationPending);
            handle
                .state()
                .set_write_status(AccessStatus::ReconfigurationPending);
        }

        let _ = self
            .send_lifecycle(|reply| LifecycleEvent::Close { reply })
            .await;

        if let Ok(handle) = self.require_handle() {
            let _ = handle
                .send_control(
                    |reply| ReplicatorControlEvent::Close { reply },
                    self.reply_timeout,
                )
                .await;
            handle.state().set_read_status(AccessStatus::NotPrimary);
            handle.state().set_write_status(AccessStatus::NotPrimary);
        }

        self.role = Role::None;
        self.replicator_handle = None;
        Ok(())
    }

    async fn handle_update_epoch(&mut self, epoch: Epoch) -> Result<()> {
        // Route entirely through replicator — it handles user notification
        self.send_replicator_control(|reply| ReplicatorControlEvent::UpdateEpoch { epoch, reply })
            .await?;
        self.epoch = epoch;
        if matches!(self.role, Role::Primary | Role::ActiveSecondary) {
            let catch_up_lsn = self
                .replicator_handle
                .as_ref()
                .map_or(0, |handle| handle.state().committed_lsn());
            self.deactivation_info = Some(ReplicaDeactivationInfo {
                epoch,
                catch_up_lsn,
            });
        }
        Ok(())
    }

    async fn handle_on_data_loss(
        &mut self,
        expected_epoch: Option<Epoch>,
    ) -> Result<DataLossAction> {
        // Route through replicator — it handles dual-query (replicator + user)
        let result = self
            .send_replicator_control_with_timeout(
                |reply| ReplicatorControlEvent::OnDataLoss {
                    expected_epoch,
                    reply,
                },
                DATA_LOSS_REPLY_TIMEOUT,
            )
            .await?;
        if result == DataLossAction::StateChanged && expected_epoch.is_some() {
            self.deactivation_info = Some(ReplicaDeactivationInfo {
                epoch: self.epoch,
                catch_up_lsn: self.require_handle()?.state().committed_lsn(),
            });
        }
        Ok(result)
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    async fn send_replicator_control<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> ReplicatorControlEvent,
    ) -> Result<T> {
        self.send_replicator_control_with_timeout(make, self.reply_timeout)
            .await
    }

    async fn send_replicator_control_with_timeout<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> ReplicatorControlEvent,
        timeout: Duration,
    ) -> Result<T> {
        let handle = self.require_handle()?;
        handle.send_control(make, timeout).await
    }

    async fn send_lifecycle<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> LifecycleEvent,
    ) -> Result<T> {
        let (tx, rx) = oneshot::channel();
        self.lifecycle_tx
            .send(make(tx))
            .await
            .map_err(|_| KubericError::Closed)?;
        match tokio::time::timeout(self.reply_timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(KubericError::Closed),
            Err(_) => {
                warn!("lifecycle event reply timed out");
                Err(KubericError::Internal("lifecycle timeout".into()))
            }
        }
    }

    fn set_status_for_role(&self, role: Role) {
        if let Some(handle) = &self.replicator_handle {
            match role {
                Role::Primary => {
                    handle.state().set_read_status(AccessStatus::Granted);
                    handle
                        .state()
                        .set_write_status(AccessStatus::ReconfigurationPending);
                }
                _ => {
                    handle.state().set_read_status(AccessStatus::NotPrimary);
                    handle.state().set_write_status(AccessStatus::NotPrimary);
                }
            }
        }
    }

    fn publish_status(&self) {
        self.status_tx.send_replace(RuntimeControlSnapshot {
            instance_id: self.instance_id.clone(),
            role: self.role,
            epoch: self.epoch,
            configuration: self.configuration.clone(),
            election_configuration: self.election_configuration.clone(),
            deactivation_info: self.deactivation_info,
            build_observation: self.build_observation.clone(),
            quorum_wait_observation: self.quorum_wait_observation.clone(),
            partition_state: self
                .replicator_handle
                .as_ref()
                .map(|handle| handle.state().clone()),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::ReplicaHandle;
    use crate::events::LifecycleEvent;
    use crate::grpc::handle::GrpcReplicaHandle;
    use crate::replicator::WalReplicator;
    use crate::types::{AgentControlVersion, CorrelatedControlActionRequest, DurableReplicaAction};

    async fn execute(handle: &GrpcReplicaHandle, action_id: &str, action: DurableReplicaAction) {
        let status = handle.get_status().await.unwrap();
        let signature = action.signature();
        handle
            .execute_correlated_control_action(CorrelatedControlActionRequest {
                protocol_version: crate::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION,
                action_id: action_id.to_string(),
                input_signature: signature,
                target_replica_id: handle.id(),
                target_instance_id: handle.instance_id(),
                expected_agent_generation: status.agent.generation,
                expected_control_version: AgentControlVersion::new(
                    status.agent.control_version.value(),
                ),
                observed_runtime_epoch: status.epoch,
                action,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn correlated_control_preserves_runtime_lifecycle_ordering() {
        let instance_id = ReplicaInstanceId::new("pod-runtime-test");
        let bundle = PodRuntime::builder(1)
            .instance_id(instance_id.clone())
            .reply_timeout(Duration::from_secs(5))
            .build()
            .await
            .unwrap();
        let shutdown = bundle.runtime.shutdown_token();
        let runtime_handle = tokio::spawn(bundle.runtime.serve());
        let mut lifecycle_rx = bundle.lifecycle_rx;
        let user_handle = tokio::spawn(async move {
            let mut replicator = None;
            while let Some(event) = lifecycle_rx.recv().await {
                match event {
                    LifecycleEvent::Open { ctx, reply } => {
                        let (state_provider_tx, _state_provider_rx) = mpsc::unbounded_channel();
                        let (handle, handles) = WalReplicator::create(
                            ctx.replica_id,
                            &ctx.data_bind,
                            ctx.fault_tx,
                            state_provider_tx,
                        )
                        .await
                        .unwrap();
                        replicator = Some(handles.replicator);
                        let _ = reply.send(Ok(handle));
                    }
                    LifecycleEvent::ChangeRole { new_role, reply } => {
                        if new_role == Role::Primary {
                            let result = replicator
                                .as_ref()
                                .unwrap()
                                .replicate(
                                    bytes::Bytes::from("from-user"),
                                    CancellationToken::new(),
                                )
                                .await;
                            assert!(matches!(result, Err(KubericError::ReconfigurationPending)));
                        }
                        let _ = reply.send(Ok(String::new()));
                    }
                    LifecycleEvent::Close { reply } => {
                        let _ = reply.send(Ok(()));
                        break;
                    }
                    LifecycleEvent::Abort => break,
                }
            }
        });
        let handle = GrpcReplicaHandle::connect(
            1,
            instance_id,
            bundle.control_address,
            "http://unused".to_string(),
        )
        .await
        .unwrap();

        execute(
            &handle,
            "open",
            DurableReplicaAction::Open {
                mode: OpenMode::New,
            },
        )
        .await;
        execute(
            &handle,
            "idle",
            DurableReplicaAction::ChangeRole {
                epoch: Epoch::new(0, 1),
                role: Role::IdleSecondary,
            },
        )
        .await;
        execute(
            &handle,
            "active",
            DurableReplicaAction::ChangeRole {
                epoch: Epoch::new(0, 1),
                role: Role::ActiveSecondary,
            },
        )
        .await;
        execute(
            &handle,
            "primary",
            DurableReplicaAction::ChangeRole {
                epoch: Epoch::new(0, 1),
                role: Role::Primary,
            },
        )
        .await;
        execute(&handle, "revoke", DurableReplicaAction::RevokeWriteStatus).await;
        assert_eq!(
            handle.get_status().await.unwrap().write_status,
            AccessStatus::ReconfigurationPending
        );
        execute(&handle, "close", DurableReplicaAction::Close).await;

        user_handle.await.unwrap();
        shutdown.cancel();
        runtime_handle.await.unwrap();
    }

    #[tokio::test]
    async fn open_reset_clears_incarnation_local_evidence() {
        let bundle = PodRuntime::builder(1).build().await.unwrap();
        let mut runtime = bundle.runtime;
        runtime.role = Role::Primary;
        runtime.epoch = Epoch::new(2, 3);
        runtime.election_configuration = Some(ReplicaElectionConfiguration {
            previous: None,
            current: ReplicaConfigurationStatus {
                mode: ReplicaConfigurationMode::Current,
                members: vec![crate::types::ReplicaConfigurationMemberStatus {
                    id: 1,
                    instance_id: runtime.instance_id.clone(),
                    role: Role::Primary,
                }],
                write_quorum: 1,
            },
        });
        runtime.deactivation_info = Some(ReplicaDeactivationInfo {
            epoch: runtime.epoch,
            catch_up_lsn: 8,
        });

        runtime.reset_lifecycle_evidence_for_open();

        assert_eq!(runtime.role, Role::Unknown);
        assert_eq!(runtime.epoch, Epoch::default());
        assert!(runtime.configuration.is_none());
        assert!(runtime.election_configuration.is_none());
        assert!(runtime.deactivation_info.is_none());
        runtime.shutdown.cancel();
    }
}
