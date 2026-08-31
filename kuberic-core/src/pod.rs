use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use tonic::transport::Server;
use tracing::{debug, info, warn};

use crate::error::{KubericError, Result};
use crate::events::{LifecycleEvent, ReplicatorControlEvent};
use crate::replicator::{OpenContext, ReplicatorHandle};
use crate::types::{
    AccessStatus, CancellationToken, DataLossAction, DurableActionCompletion,
    DurableActionObservation, DurableActionResult, DurableActionState, DurableReplicaAction, Epoch,
    Lsn, OpenMode, ReplicaConfigurationMode, ReplicaConfigurationStatus, ReplicaDeactivationInfo,
    ReplicaElectionConfiguration, ReplicaId, ReplicaInfo, ReplicaInstanceId, ReplicaSetConfig,
    ReplicaSetQuorumMode, Role,
};

const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const BUILD_REPLY_TIMEOUT: Duration = Duration::from_secs(600);

// ---------------------------------------------------------------------------
// RuntimeCommand — what the gRPC control server sends to the runtime
// ---------------------------------------------------------------------------

/// Commands sent by the gRPC ControlServer to the PodRuntime.
/// The runtime processes these with correct ordering (replicator + user events).
pub enum RuntimeCommand {
    Open {
        mode: OpenMode,
        reply: oneshot::Sender<Result<()>>,
    },
    Close {
        reply: oneshot::Sender<Result<()>>,
    },
    ChangeRole {
        epoch: Epoch,
        role: Role,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateEpoch {
        epoch: Epoch,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateCatchUpConfiguration {
        current: ReplicaSetConfig,
        previous: ReplicaSetConfig,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateCurrentConfiguration {
        current: ReplicaSetConfig,
        reply: oneshot::Sender<Result<()>>,
    },
    WaitForCatchUpQuorum {
        mode: ReplicaSetQuorumMode,
        reply: oneshot::Sender<Result<()>>,
    },
    BuildReplica {
        replica: ReplicaInfo,
        reply: oneshot::Sender<Result<()>>,
    },
    RemoveReplica {
        replica_id: ReplicaId,
        instance_id: ReplicaInstanceId,
        reply: oneshot::Sender<Result<()>>,
    },
    OnDataLoss {
        reply: oneshot::Sender<Result<DataLossAction>>,
    },
    RevokeWriteStatus {
        reply: oneshot::Sender<Result<()>>,
    },
    ExecuteDurableAction {
        action_id: String,
        action: DurableReplicaAction,
        reply: oneshot::Sender<Result<()>>,
    },
    GetStatus {
        reply: oneshot::Sender<StatusInfo>,
    },
}

/// Status info returned by GetStatus.
pub struct StatusInfo {
    pub instance_id: ReplicaInstanceId,
    pub role: Role,
    pub epoch: Epoch,
    pub current_progress: Lsn,
    pub catch_up_capability: Option<Lsn>,
    pub committed_lsn: Lsn,
    pub healthy: bool,
    pub write_status: AccessStatus,
    pub configuration: Option<ReplicaConfigurationStatus>,
    pub election_configuration: Option<ReplicaElectionConfiguration>,
    pub deactivation_info: Option<ReplicaDeactivationInfo>,
    pub last_completed_action: Option<DurableActionCompletion>,
    pub durable_action: Option<DurableActionObservation>,
    pub active_replica_connections: Vec<crate::types::ReplicaConnectionStatus>,
}

// ---------------------------------------------------------------------------
// PodRuntime
// ---------------------------------------------------------------------------

pub struct PodRuntime {
    lifecycle_tx: mpsc::Sender<LifecycleEvent>,
    cmd_rx: mpsc::Receiver<RuntimeCommand>,
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
    deactivation_info: Arc<Mutex<Option<ReplicaDeactivationInfo>>>,
    durable_action: Arc<Mutex<Option<DurableActionObservation>>>,
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

    pub async fn build(self) -> Result<PodRuntimeBundle> {
        let shutdown = CancellationToken::new();
        let (lifecycle_tx, lifecycle_rx) = mpsc::channel(16);
        let (cmd_tx, cmd_rx) = mpsc::channel(16);

        // Control plane gRPC server (runtime-owned, unchanged)
        // PartitionState is not available yet — it's created by the replicator
        // at Open time. ControlServer needs to work without it initially.
        let control_server = crate::grpc::server::ControlServer::new(self.replica_id, cmd_tx);
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
            deactivation_info: Arc::new(Mutex::new(None)),
            durable_action: Arc::new(Mutex::new(None)),
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
        while let Some(cmd) = self.cmd_rx.recv().await {
            match cmd {
                RuntimeCommand::Open { mode, reply } => {
                    let _ = reply.send(self.handle_open(mode).await);
                }
                RuntimeCommand::Close { reply } => {
                    let _ = reply.send(self.handle_close().await);
                    break;
                }
                RuntimeCommand::ChangeRole { epoch, role, reply } => {
                    let _ = reply.send(match self.require_handle() {
                        Ok(_) => self.handle_change_role(epoch, role).await,
                        Err(e) => Err(e),
                    });
                }
                RuntimeCommand::UpdateEpoch { epoch, reply } => {
                    let _ = reply.send(self.handle_update_epoch(epoch).await);
                }
                RuntimeCommand::UpdateCatchUpConfiguration {
                    current,
                    previous,
                    reply,
                } => {
                    let observed_configuration = ReplicaConfigurationStatus::from_config(
                        ReplicaConfigurationMode::CatchUp,
                        &current,
                    );
                    let result = self
                        .send_replicator_control(|r| {
                            ReplicatorControlEvent::UpdateCatchUpConfiguration {
                                current,
                                previous,
                                reply: r,
                            }
                        })
                        .await;
                    if result.is_ok() && self.role == Role::Primary {
                        if let Some(handle) = &self.replicator_handle {
                            handle.state().set_write_status(AccessStatus::Granted);
                        }
                        self.configuration = Some(observed_configuration);
                    }
                    let _ = reply.send(result);
                }
                RuntimeCommand::UpdateCurrentConfiguration { current, reply } => {
                    let observed_configuration = ReplicaConfigurationStatus::from_config(
                        ReplicaConfigurationMode::Current,
                        &current,
                    );
                    let result = self
                        .send_replicator_control(|r| {
                            ReplicatorControlEvent::UpdateCurrentConfiguration { current, reply: r }
                        })
                        .await;
                    if result.is_ok() && self.role == Role::Primary {
                        if let Some(handle) = &self.replicator_handle {
                            handle.state().set_write_status(AccessStatus::Granted);
                        }
                        self.configuration = Some(observed_configuration);
                    }
                    let _ = reply.send(result);
                }
                RuntimeCommand::WaitForCatchUpQuorum { mode, reply } => {
                    let _ = reply.send(
                        self.send_replicator_control(|r| {
                            ReplicatorControlEvent::WaitForCatchUpQuorum { mode, reply: r }
                        })
                        .await,
                    );
                }
                RuntimeCommand::BuildReplica { replica, reply } => {
                    // Copy protocol now runs inside the replicator actor.
                    // The actor spawns a task and replies when copy completes.
                    let _ = reply.send(
                        self.send_replicator_control(|r| ReplicatorControlEvent::BuildReplica {
                            replica,
                            reply: r,
                        })
                        .await,
                    );
                }
                RuntimeCommand::RemoveReplica {
                    replica_id,
                    instance_id,
                    reply,
                } => {
                    let _ = reply.send(
                        self.send_replicator_control(|r| ReplicatorControlEvent::RemoveReplica {
                            replica_id,
                            instance_id,
                            reply: r,
                        })
                        .await,
                    );
                }
                RuntimeCommand::OnDataLoss { reply } => {
                    let _ = reply.send(self.handle_on_data_loss().await);
                }
                RuntimeCommand::RevokeWriteStatus { reply } => {
                    info!("revoking write status for switchover");
                    if let Some(handle) = &self.replicator_handle {
                        handle
                            .state()
                            .set_write_status(AccessStatus::ReconfigurationPending);
                    }
                    let _ = reply.send(Ok(()));
                }
                RuntimeCommand::ExecuteDurableAction {
                    action_id,
                    action,
                    reply,
                } => {
                    let _ = reply.send(self.handle_durable_action(action_id, action).await);
                }
                RuntimeCommand::GetStatus { reply } => {
                    let handle = self.replicator_handle.as_ref();
                    let durable_action = self.durable_action.lock().unwrap().clone();
                    let last_completed_action = durable_action.as_ref().and_then(|action| {
                        (action.state == DurableActionState::Completed).then(|| {
                            DurableActionCompletion {
                                action_id: action.action_id.clone(),
                                signature: action.signature.clone(),
                                result: action.result,
                            }
                        })
                    });
                    let _ = reply.send(StatusInfo {
                        instance_id: self.instance_id.clone(),
                        role: self.role,
                        epoch: self.epoch,
                        current_progress: handle.map_or(0, |h| h.state().current_progress()),
                        catch_up_capability: handle
                            .and_then(|h| h.state().observed_catch_up_capability()),
                        committed_lsn: handle.map_or(0, |h| h.state().committed_lsn()),
                        healthy: handle.is_some(),
                        write_status: handle
                            .map_or(AccessStatus::NotPrimary, |h| h.state().write_status()),
                        configuration: self.configuration.clone(),
                        election_configuration: self.election_configuration.clone(),
                        deactivation_info: *self.deactivation_info.lock().unwrap(),
                        last_completed_action,
                        durable_action,
                        active_replica_connections: handle
                            .map_or_else(Vec::new, |h| h.state().active_replica_connections()),
                    });
                }
            }
        }
        self.shutdown.cancel();
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

    async fn handle_durable_action(
        &mut self,
        action_id: String,
        action: DurableReplicaAction,
    ) -> Result<()> {
        let signature = action.signature();
        {
            let current = self.durable_action.lock().unwrap();
            if let Some(observed) = current.as_ref() {
                if observed.action_id == action_id {
                    if observed.signature != signature {
                        return Err(KubericError::Internal(
                            format!(
                                "durable action ID {action_id} was reused with different input"
                            )
                            .into(),
                        ));
                    }
                    return match observed.state {
                        DurableActionState::Scheduled
                        | DurableActionState::InProgress
                        | DurableActionState::Completed => Ok(()),
                        DurableActionState::Failed => Err(KubericError::Internal(
                            observed
                                .error
                                .clone()
                                .unwrap_or_else(|| "durable action failed".to_string())
                                .into(),
                        )),
                    };
                } else if matches!(
                    observed.state,
                    DurableActionState::Scheduled | DurableActionState::InProgress
                ) {
                    return Err(KubericError::Internal(
                        format!("durable action {} is still in progress", observed.action_id)
                            .into(),
                    ));
                }
            }
        }

        self.set_durable_action(&action_id, &signature, DurableActionState::Scheduled, None);

        if let DurableReplicaAction::BuildReplica { replica } = action.clone() {
            let handle = match self.require_handle() {
                Ok(handle) => handle.clone(),
                Err(error) => {
                    self.set_durable_action(
                        &action_id,
                        &signature,
                        DurableActionState::Failed,
                        Some(error.to_string()),
                    );
                    return Err(error);
                }
            };
            let durable_action = self.durable_action.clone();
            let action_id_for_task = action_id.clone();
            let signature_for_task = signature.clone();
            tokio::spawn(async move {
                Self::set_durable_action_state(
                    &durable_action,
                    &action_id_for_task,
                    &signature_for_task,
                    DurableActionState::InProgress,
                    None,
                );
                let result = handle
                    .send_control(
                        |reply| ReplicatorControlEvent::BuildReplica { replica, reply },
                        BUILD_REPLY_TIMEOUT,
                    )
                    .await;
                match result {
                    Ok(()) => Self::set_durable_action_state(
                        &durable_action,
                        &action_id_for_task,
                        &signature_for_task,
                        DurableActionState::Completed,
                        None,
                    ),
                    Err(error) => Self::set_durable_action_state(
                        &durable_action,
                        &action_id_for_task,
                        &signature_for_task,
                        DurableActionState::Failed,
                        Some(error.to_string()),
                    ),
                }
            });
            return Ok(());
        }

        if let DurableReplicaAction::OnDataLoss { epoch } = action.clone() {
            if self.epoch != epoch {
                let error = KubericError::Internal(
                    format!(
                        "data-loss action epoch {:?} does not match runtime epoch {:?}",
                        epoch, self.epoch
                    )
                    .into(),
                );
                self.set_durable_action(
                    &action_id,
                    &signature,
                    DurableActionState::Failed,
                    Some(error.to_string()),
                );
                warn!(
                    action_id,
                    expected_epoch = ?epoch,
                    runtime_epoch = ?self.epoch,
                    "rejecting durable data-loss action at mismatched epoch"
                );
                return Err(error);
            }

            let handle = match self.require_handle() {
                Ok(handle) => handle.clone(),
                Err(error) => {
                    self.set_durable_action(
                        &action_id,
                        &signature,
                        DurableActionState::Failed,
                        Some(error.to_string()),
                    );
                    return Err(error);
                }
            };
            let durable_action = self.durable_action.clone();
            let action_id_for_task = action_id.clone();
            let signature_for_task = signature.clone();
            let reply_timeout = self.reply_timeout;
            let deactivation_info = self.deactivation_info.clone();
            info!(
                action_id,
                expected_epoch = ?epoch,
                "scheduling durable data-loss callback"
            );
            tokio::spawn(async move {
                debug!(action_id = %action_id_for_task, "durable data-loss callback in progress");
                Self::set_durable_action_state(
                    &durable_action,
                    &action_id_for_task,
                    &signature_for_task,
                    DurableActionState::InProgress,
                    None,
                );
                let result = handle
                    .send_control(
                        |reply| ReplicatorControlEvent::OnDataLoss {
                            expected_epoch: Some(epoch),
                            reply,
                        },
                        reply_timeout,
                    )
                    .await;
                match result {
                    Ok(result) => {
                        if result == DataLossAction::StateChanged {
                            *deactivation_info.lock().unwrap() = Some(ReplicaDeactivationInfo {
                                epoch,
                                catch_up_lsn: handle.state().committed_lsn(),
                            });
                        }
                        info!(
                            action_id = %action_id_for_task,
                            state_changed = result == DataLossAction::StateChanged,
                            "durable data-loss callback completed"
                        );
                        Self::set_durable_action_state_with_result(
                            &durable_action,
                            &action_id_for_task,
                            &signature_for_task,
                            DurableActionState::Completed,
                            None,
                            Some(DurableActionResult::DataLoss(result)),
                        );
                    }
                    Err(error) => {
                        warn!(
                            action_id = %action_id_for_task,
                            "durable data-loss callback failed"
                        );
                        Self::set_durable_action_state(
                            &durable_action,
                            &action_id_for_task,
                            &signature_for_task,
                            DurableActionState::Failed,
                            Some(error.to_string()),
                        );
                    }
                }
            });
            return Ok(());
        }

        self.set_durable_action(&action_id, &signature, DurableActionState::InProgress, None);
        let result = match action {
            DurableReplicaAction::Open { mode } => self.handle_open(mode).await,
            DurableReplicaAction::Close => self.handle_close().await,
            DurableReplicaAction::RevokeWriteStatus => {
                let handle = self.require_handle()?;
                handle
                    .state()
                    .set_write_status(AccessStatus::ReconfigurationPending);
                Ok(())
            }
            DurableReplicaAction::ChangeRole { epoch, role } => {
                self.require_handle()?;
                self.handle_change_role(epoch, role).await
            }
            DurableReplicaAction::UpdateEpoch { epoch } => self.handle_update_epoch(epoch).await,
            DurableReplicaAction::UpdateCatchUpConfiguration { current, previous } => {
                let result = self
                    .send_replicator_control(|reply| {
                        ReplicatorControlEvent::UpdateCatchUpConfiguration {
                            current: current.clone(),
                            previous,
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
                    self.configuration = Some(ReplicaConfigurationStatus::from_config(
                        ReplicaConfigurationMode::CatchUp,
                        &current,
                    ));
                }
                result
            }
            DurableReplicaAction::WaitForCatchUpQuorum { mode } => {
                self.send_replicator_control(|reply| ReplicatorControlEvent::WaitForCatchUpQuorum {
                    mode,
                    reply,
                })
                .await
            }
            DurableReplicaAction::UpdateCurrentConfiguration { current } => {
                let result = self
                    .send_replicator_control(|reply| {
                        ReplicatorControlEvent::UpdateCurrentConfiguration {
                            current: current.clone(),
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
                    self.configuration = Some(ReplicaConfigurationStatus::from_config(
                        ReplicaConfigurationMode::Current,
                        &current,
                    ));
                }
                result
            }
            DurableReplicaAction::RemoveReplica {
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
            DurableReplicaAction::RecordElectionConfiguration { configuration } => {
                self.election_configuration = Some(configuration);
                Ok(())
            }
            DurableReplicaAction::BuildReplica { .. } | DurableReplicaAction::OnDataLoss { .. } => {
                unreachable!()
            }
        };

        match &result {
            Ok(()) => {
                self.set_durable_action(&action_id, &signature, DurableActionState::Completed, None)
            }
            Err(error) => self.set_durable_action(
                &action_id,
                &signature,
                DurableActionState::Failed,
                Some(error.to_string()),
            ),
        }
        result
    }

    fn set_durable_action(
        &self,
        action_id: &str,
        signature: &str,
        state: DurableActionState,
        error: Option<String>,
    ) {
        Self::set_durable_action_state(&self.durable_action, action_id, signature, state, error);
    }

    async fn handle_open(&mut self, mode: OpenMode) -> Result<()> {
        if self.replicator_handle.is_some() {
            return Err(KubericError::Internal("already opened".into()));
        }
        self.reset_lifecycle_evidence_for_open();

        // 1. Send OpenContext to user, receive ReplicatorHandle back
        let (fault_tx, _fault_rx) = mpsc::channel(4);

        let handle: ReplicatorHandle = self
            .send_lifecycle(|reply| LifecycleEvent::Open {
                ctx: OpenContext {
                    replica_id: self.replica_id,
                    open_mode: mode,
                    data_bind: self.data_bind.clone(),
                    token: self.shutdown.child_token(),
                    fault_tx,
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
        *self.deactivation_info.lock().unwrap() = None;
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
            *self.deactivation_info.lock().unwrap() = Some(ReplicaDeactivationInfo {
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
            *self.deactivation_info.lock().unwrap() = Some(ReplicaDeactivationInfo {
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

    fn set_durable_action_state(
        slot: &Arc<Mutex<Option<DurableActionObservation>>>,
        action_id: &str,
        signature: &str,
        state: DurableActionState,
        error: Option<String>,
    ) {
        Self::set_durable_action_state_with_result(slot, action_id, signature, state, error, None);
    }

    fn set_durable_action_state_with_result(
        slot: &Arc<Mutex<Option<DurableActionObservation>>>,
        action_id: &str,
        signature: &str,
        state: DurableActionState,
        error: Option<String>,
        result: Option<DurableActionResult>,
    ) {
        *slot.lock().unwrap() = Some(DurableActionObservation {
            action_id: action_id.to_string(),
            signature: signature.to_string(),
            state,
            error,
            result,
        });
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
            *self.deactivation_info.lock().unwrap() = Some(ReplicaDeactivationInfo {
                epoch,
                catch_up_lsn,
            });
        }
        Ok(())
    }

    async fn handle_on_data_loss(&mut self) -> Result<DataLossAction> {
        // Route through replicator — it handles dual-query (replicator + user)
        self.send_replicator_control(|reply| ReplicatorControlEvent::OnDataLoss {
            expected_epoch: None,
            reply,
        })
        .await
    }

    // -----------------------------------------------------------------------
    // Helpers
    // -----------------------------------------------------------------------

    async fn send_replicator_control<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<T>>) -> ReplicatorControlEvent,
    ) -> Result<T> {
        let handle = self.require_handle()?;
        handle.send_control(make, self.reply_timeout).await
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::LifecycleEvent;
    use crate::replicator::WalReplicator;

    #[tokio::test]
    async fn test_pod_runtime_user_lifecycle() {
        let bundle = PodRuntime::builder(1)
            .reply_timeout(Duration::from_secs(5))
            .build()
            .await
            .unwrap();

        let runtime = bundle.runtime;
        let mut lifecycle_rx = bundle.lifecycle_rx;

        // Spawn user event loop — creates replicator at Open
        let user_handle = tokio::spawn(async move {
            let mut replicator = None;

            while let Some(event) = lifecycle_rx.recv().await {
                match event {
                    LifecycleEvent::Open { ctx, reply } => {
                        // User creates channel and replicator
                        let (sp_tx, _sp_rx) = mpsc::unbounded_channel();
                        let (handle, handles) = WalReplicator::create(
                            ctx.replica_id,
                            &ctx.data_bind,
                            ctx.fault_tx.clone(),
                            sp_tx,
                        )
                        .await
                        .unwrap();
                        replicator = Some(handles.replicator);
                        let _ = reply.send(Ok(handle));
                    }
                    LifecycleEvent::ChangeRole { new_role, reply } => {
                        if new_role == Role::Primary {
                            let r = replicator.as_ref().unwrap();
                            let result = r
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

        // Spawn the runtime command loop
        let runtime_handle = tokio::spawn(runtime.serve());

        // Drive lifecycle via the gRPC control server (simulating operator)
        let mut client = crate::proto::replicator_control_client::ReplicatorControlClient::connect(
            bundle.control_address.clone(),
        )
        .await
        .unwrap();

        // Open
        client
            .open(crate::proto::OpenRequest { mode: 0 })
            .await
            .unwrap();

        // ChangeRole Idle → Active → Primary
        client
            .change_role(crate::proto::ChangeRoleRequest {
                epoch: Some(crate::proto::EpochProto {
                    data_loss_number: 0,
                    configuration_number: 1,
                }),
                role: crate::proto::RoleProto::RoleIdleSecondary as i32,
            })
            .await
            .unwrap();

        client
            .change_role(crate::proto::ChangeRoleRequest {
                epoch: Some(crate::proto::EpochProto {
                    data_loss_number: 0,
                    configuration_number: 1,
                }),
                role: crate::proto::RoleProto::RoleActiveSecondary as i32,
            })
            .await
            .unwrap();

        client
            .change_role(crate::proto::ChangeRoleRequest {
                epoch: Some(crate::proto::EpochProto {
                    data_loss_number: 0,
                    configuration_number: 1,
                }),
                role: crate::proto::RoleProto::RolePrimary as i32,
            })
            .await
            .unwrap();

        let durable_revoke = crate::proto::ExecuteDurableActionRequest {
            action_id: "operation:1:revoke".to_string(),
            action: Some(
                crate::proto::execute_durable_action_request::Action::RevokeWriteStatus(
                    crate::proto::RevokeWriteStatusRequest {},
                ),
            ),
        };
        client
            .execute_durable_action(durable_revoke.clone())
            .await
            .unwrap();
        client.execute_durable_action(durable_revoke).await.unwrap();
        let status = client
            .get_status(crate::proto::GetStatusRequest {})
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            status.write_status,
            crate::proto::AccessStatusProto::AccessReconfigurationPending as i32
        );
        assert_eq!(status.last_completed_action_id, "operation:1:revoke");
        assert_eq!(
            status.last_completed_action_signature,
            "revoke-write-status"
        );
        assert_eq!(
            status.durable_action_state,
            crate::proto::DurableActionStateProto::DurableActionCompleted as i32
        );
        assert_eq!(status.durable_action_id, "operation:1:revoke");

        let reused = client
            .execute_durable_action(crate::proto::ExecuteDurableActionRequest {
                action_id: "operation:1:revoke".to_string(),
                action: Some(
                    crate::proto::execute_durable_action_request::Action::UpdateEpoch(
                        crate::proto::UpdateEpochRequest {
                            epoch: Some(crate::proto::EpochProto {
                                data_loss_number: 0,
                                configuration_number: 2,
                            }),
                        },
                    ),
                ),
            })
            .await;
        assert!(reused.is_err());

        // Demote
        client
            .change_role(crate::proto::ChangeRoleRequest {
                epoch: Some(crate::proto::EpochProto {
                    data_loss_number: 0,
                    configuration_number: 2,
                }),
                role: crate::proto::RoleProto::RoleActiveSecondary as i32,
            })
            .await
            .unwrap();

        // Close
        client.close(crate::proto::CloseRequest {}).await.unwrap();

        user_handle.await.unwrap();

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
        *runtime.deactivation_info.lock().unwrap() = Some(ReplicaDeactivationInfo {
            epoch: runtime.epoch,
            catch_up_lsn: 8,
        });

        runtime.reset_lifecycle_evidence_for_open();

        assert_eq!(runtime.role, Role::Unknown);
        assert_eq!(runtime.epoch, Epoch::default());
        assert!(runtime.configuration.is_none());
        assert!(runtime.election_configuration.is_none());
        assert!(runtime.deactivation_info.lock().unwrap().is_none());
        runtime.shutdown.cancel();
    }
}
