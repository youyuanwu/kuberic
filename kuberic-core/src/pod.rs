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
    AccessStatus, CancellationToken, DataLossAction, Epoch, OpenMode, ReplicaConfigurationMode,
    ReplicaConfigurationStatus, ReplicaDeactivationInfo, ReplicaElectionConfiguration, ReplicaId,
    ReplicaInfo, ReplicaInstanceId, ReplicaSetConfig, ReplicaSetQuorumMode, Role,
};

const DEFAULT_REPLY_TIMEOUT: Duration = Duration::from_secs(30);
const BUILD_REPLY_TIMEOUT: Duration = Duration::from_secs(600);
// ---------------------------------------------------------------------------
// RuntimeEffectCommand — what the replica agent sends to the runtime
// ---------------------------------------------------------------------------

/// One ordered service/replicator effect delegated by [`ReplicaAgent`].
pub enum RuntimeEffect {
    Open {
        mode: OpenMode,
    },
    Close {
        terminate_runtime: bool,
    },
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
        observe_on_secondary: bool,
    },
    UpdateCurrentConfiguration {
        current: ReplicaSetConfig,
        observe_on_secondary: bool,
    },
    WaitForCatchUpQuorum {
        mode: ReplicaSetQuorumMode,
    },
    BuildReplica {
        replica: ReplicaInfo,
        extended_timeout: bool,
    },
    RemoveReplica {
        replica_id: ReplicaId,
        instance_id: ReplicaInstanceId,
    },
    OnDataLoss {
        expected_epoch: Option<Epoch>,
    },
    RevokeWriteStatus {
        require_open: bool,
    },
    RecordElectionConfiguration {
        configuration: ReplicaElectionConfiguration,
    },
}

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
    pub partition_state: Option<Arc<PartitionState>>,
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
            partition_state: None,
        };
        let (status_tx, status_rx) = watch::channel(initial_status);

        // Control plane gRPC server routes through the pod-local agent.
        // PartitionState is not available yet — it's created by the replicator
        // at Open time. ControlServer needs to work without it initially.
        let control_server = crate::grpc::server::ControlServer::new(self.replica_id, agent_tx);
        let replica_agent = crate::replica_agent::ReplicaAgent::new(
            self.replica_id,
            self.instance_id.clone(),
            agent_rx,
            runtime_tx,
            status_rx,
            fault_rx,
            shutdown.child_token(),
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
        while let Some(command) = self.cmd_rx.recv().await {
            let terminate_runtime = matches!(
                &command.effect,
                RuntimeEffect::Close {
                    terminate_runtime: true
                }
            );
            let result = self.execute_effect(command.effect).await;
            self.publish_status();
            let _ = command.reply.send(result);
            if terminate_runtime {
                break;
            }
        }
        self.shutdown.cancel();
    }

    async fn execute_effect(&mut self, effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        let result = match effect {
            RuntimeEffect::Open { mode } => self.handle_open(mode).await,
            RuntimeEffect::Close { .. } => self.handle_close().await,
            RuntimeEffect::ChangeRole { epoch, role } => {
                self.require_handle()?;
                self.handle_change_role(epoch, role).await
            }
            RuntimeEffect::UpdateEpoch { epoch } => self.handle_update_epoch(epoch).await,
            RuntimeEffect::UpdateCatchUpConfiguration {
                current,
                previous,
                observe_on_secondary,
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
                            reply,
                        }
                    })
                    .await;
                if result.is_ok() && self.role == Role::Primary {
                    self.require_handle()?
                        .state()
                        .set_write_status(AccessStatus::Granted);
                }
                if result.is_ok() && (observe_on_secondary || self.role == Role::Primary) {
                    self.configuration = Some(observed_configuration);
                }
                result
            }
            RuntimeEffect::UpdateCurrentConfiguration {
                current,
                observe_on_secondary,
            } => {
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
                if result.is_ok() && (observe_on_secondary || self.role == Role::Primary) {
                    self.configuration = Some(observed_configuration);
                }
                result
            }
            RuntimeEffect::WaitForCatchUpQuorum { mode } => {
                self.send_replicator_control(|reply| ReplicatorControlEvent::WaitForCatchUpQuorum {
                    mode,
                    reply,
                })
                .await
            }
            RuntimeEffect::BuildReplica {
                replica,
                extended_timeout,
            } => {
                let timeout = if extended_timeout {
                    BUILD_REPLY_TIMEOUT
                } else {
                    self.reply_timeout
                };
                self.send_replicator_control_with_timeout(
                    |reply| ReplicatorControlEvent::BuildReplica { replica, reply },
                    timeout,
                )
                .await
            }
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
                    .handle_on_data_loss(expected_epoch)
                    .await
                    .map(RuntimeEffectResult::DataLoss);
            }
            RuntimeEffect::RevokeWriteStatus { require_open } => {
                info!("revoking write status for switchover");
                if require_open {
                    self.require_handle()?
                        .state()
                        .set_write_status(AccessStatus::ReconfigurationPending);
                } else if let Some(handle) = &self.replicator_handle {
                    handle
                        .state()
                        .set_write_status(AccessStatus::ReconfigurationPending);
                }
                Ok(())
            }
            RuntimeEffect::RecordElectionConfiguration { configuration } => {
                self.election_configuration = Some(configuration);
                Ok(())
            }
        };
        result.map(|()| RuntimeEffectResult::Unit)
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
            .send_replicator_control(|reply| ReplicatorControlEvent::OnDataLoss {
                expected_epoch,
                reply,
            })
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

        let empty_action_id = client
            .execute_durable_action(crate::proto::ExecuteDurableActionRequest {
                action_id: String::new(),
                action: Some(
                    crate::proto::execute_durable_action_request::Action::RevokeWriteStatus(
                        crate::proto::RevokeWriteStatusRequest {},
                    ),
                ),
            })
            .await
            .unwrap_err();
        assert_eq!(empty_action_id.code(), tonic::Code::InvalidArgument);

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
        assert_eq!(
            status.replica_agent_protocol_version,
            crate::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION
        );
        assert!(!status.agent_generation.is_empty());

        let correlated_revoke = crate::proto::ExecuteCorrelatedControlActionRequest {
            protocol_version: crate::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION,
            action_id: "operation:2:revoke".to_string(),
            input_signature: "revoke-write-status".to_string(),
            target_replica_id: 1,
            target_instance_id: status.instance_id.clone(),
            expected_agent_generation: status.agent_generation.clone(),
            expected_agent_control_version: Some(status.agent_control_version),
            observed_runtime_epoch: status.epoch,
            action: Some(
                crate::proto::execute_correlated_control_action_request::Action::RevokeWriteStatus(
                    crate::proto::RevokeWriteStatusRequest {},
                ),
            ),
        };
        let acknowledgement = client
            .execute_correlated_control_action(correlated_revoke.clone())
            .await
            .unwrap()
            .into_inner()
            .observation
            .unwrap();
        assert_eq!(
            acknowledgement.admission,
            crate::proto::CorrelatedActionAdmissionProto::CorrelatedActionAdmissionVersioned as i32
        );
        assert_eq!(
            acknowledgement.state,
            crate::proto::DurableActionStateProto::DurableActionCompleted as i32
        );
        let replay = client
            .execute_correlated_control_action(correlated_revoke.clone())
            .await
            .unwrap()
            .into_inner()
            .observation
            .unwrap();
        assert_eq!(replay, acknowledgement);

        let mut unsupported = correlated_revoke;
        unsupported.protocol_version += 1;
        let error = client
            .execute_correlated_control_action(unsupported)
            .await
            .unwrap_err();
        assert_eq!(error.code(), tonic::Code::Unimplemented);

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
