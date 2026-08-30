use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use tonic::transport::Channel;

use crate::driver::ReplicaHandle;
use crate::error::{KubericError, Result};
use crate::proto::replicator_control_client::ReplicatorControlClient;
use crate::proto::*;
use crate::types::{
    DataLossAction, DurableActionCompletion, DurableActionObservation, DurableActionState,
    DurableReplicaAction, Epoch, Lsn, OpenMode, ReplicaConnectionStatus, ReplicaId, ReplicaInfo,
    ReplicaInstanceId, ReplicaSetConfig, ReplicaSetQuorumMode, ReplicaStatusInfo, Role,
};

/// Implements `ReplicaHandle` by calling a remote pod's gRPC `ReplicatorControl` service.
/// Used by the operator to drive remote replicas.
pub struct GrpcReplicaHandle {
    id: ReplicaId,
    instance_id: ReplicaInstanceId,
    client: ReplicatorControlClient<Channel>,
    /// The data plane address (secondary gRPC server for replication streams).
    data_address: String,
    current_progress: AtomicI64,
    catch_up_capability: AtomicI64,
}

impl GrpcReplicaHandle {
    pub async fn connect(
        id: ReplicaId,
        instance_id: ReplicaInstanceId,
        control_address: String,
        data_address: String,
    ) -> Result<Self> {
        let channel = Channel::from_shared(control_address)
            .map_err(|e| KubericError::Internal(Box::new(e)))?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await
            .map_err(|e| KubericError::Internal(Box::new(e)))?;

        Ok(Self {
            id,
            instance_id,
            client: ReplicatorControlClient::new(channel),
            data_address,
            current_progress: AtomicI64::new(0),
            catch_up_capability: AtomicI64::new(0),
        })
    }

    fn map_err(e: tonic::Status) -> KubericError {
        match e.code() {
            tonic::Code::FailedPrecondition => KubericError::NotPrimary,
            tonic::Code::Unavailable => {
                if e.message().contains("no write quorum") {
                    KubericError::NoWriteQuorum
                } else if e.message().contains("reconfiguration") {
                    KubericError::ReconfigurationPending
                } else {
                    KubericError::Internal(Box::new(e))
                }
            }
            _ => KubericError::Internal(Box::new(e)),
        }
    }
}

#[async_trait]
impl ReplicaHandle for GrpcReplicaHandle {
    fn id(&self) -> ReplicaId {
        self.id
    }

    fn instance_id(&self) -> ReplicaInstanceId {
        self.instance_id.clone()
    }

    async fn open(&self, mode: OpenMode) -> Result<()> {
        let mut client = self.client.clone();
        client
            .open(OpenRequest {
                mode: OpenModeProto::from(mode) as i32,
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        let mut client = self.client.clone();
        client.close(CloseRequest {}).await.map_err(Self::map_err)?;
        Ok(())
    }

    fn abort(&self) {
        // gRPC doesn't have fire-and-forget; best effort
        let mut client = self.client.clone();
        tokio::spawn(async move {
            let _ = client.close(CloseRequest {}).await;
        });
    }

    async fn change_role(&self, epoch: Epoch, role: Role) -> Result<()> {
        let mut client = self.client.clone();
        client
            .change_role(ChangeRoleRequest {
                epoch: Some(epoch.into()),
                role: RoleProto::from(role) as i32,
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        let mut client = self.client.clone();
        client
            .update_epoch(UpdateEpochRequest {
                epoch: Some(epoch.into()),
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    fn current_progress(&self) -> Lsn {
        self.current_progress.load(Ordering::Acquire)
    }

    fn catch_up_capability(&self) -> Lsn {
        self.catch_up_capability.load(Ordering::Acquire)
    }

    async fn on_data_loss(&self) -> Result<DataLossAction> {
        let mut client = self.client.clone();
        let resp = client
            .on_data_loss(OnDataLossRequest {})
            .await
            .map_err(Self::map_err)?;
        if resp.into_inner().state_changed {
            Ok(DataLossAction::StateChanged)
        } else {
            Ok(DataLossAction::None)
        }
    }

    async fn update_catch_up_configuration(
        &self,
        current: ReplicaSetConfig,
        previous: ReplicaSetConfig,
    ) -> Result<()> {
        let mut client = self.client.clone();
        client
            .update_catch_up_configuration(UpdateCatchUpConfigRequest {
                current: Some(current.into()),
                previous: Some(previous.into()),
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn update_current_configuration(&self, current: ReplicaSetConfig) -> Result<()> {
        let mut client = self.client.clone();
        client
            .update_current_configuration(UpdateCurrentConfigRequest {
                current: Some(current.into()),
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        let mut client = self.client.clone();
        client
            .wait_for_catch_up_quorum(WaitForCatchUpQuorumRequest {
                mode: QuorumModeProto::from(mode) as i32,
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn build_replica(&self, replica: ReplicaInfo) -> Result<()> {
        let mut client = self.client.clone();
        client
            .build_replica(BuildReplicaRequest {
                replica: Some(replica.into()),
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn remove_replica(
        &self,
        replica_id: ReplicaId,
        instance_id: ReplicaInstanceId,
    ) -> Result<()> {
        let mut client = self.client.clone();
        client
            .remove_replica(RemoveReplicaRequest {
                replica_id,
                instance_id: instance_id.to_string(),
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    async fn revoke_write_status(&self) -> Result<()> {
        let mut client = self.client.clone();
        client
            .revoke_write_status(RevokeWriteStatusRequest {})
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }

    fn replicator_address(&self) -> String {
        self.data_address.clone()
    }

    async fn get_status(&self) -> Result<ReplicaStatusInfo> {
        let mut client = self.client.clone();
        let resp = client
            .get_status(GetStatusRequest {})
            .await
            .map_err(Self::map_err)?;
        let inner = resp.into_inner();
        let epoch = inner.epoch.map(Epoch::from).unwrap_or(Epoch::new(0, 0));
        let role = Role::from(inner.role);

        // Update cached progress as side effect
        self.current_progress
            .store(inner.current_progress, Ordering::Release);
        self.catch_up_capability
            .store(inner.catch_up_capability, Ordering::Release);

        Ok(ReplicaStatusInfo {
            instance_id: ReplicaInstanceId::new(inner.instance_id),
            role,
            epoch,
            current_progress: inner.current_progress,
            healthy: inner.healthy,
            write_status: crate::types::AccessStatus::from(inner.write_status),
            configuration: inner.configuration.map(Into::into),
            last_completed_action: if inner.last_completed_action_id.is_empty() {
                None
            } else {
                Some(DurableActionCompletion {
                    action_id: inner.last_completed_action_id,
                    signature: inner.last_completed_action_signature,
                })
            },
            durable_action: if inner.durable_action_id.is_empty() {
                None
            } else {
                let state = match DurableActionStateProto::try_from(inner.durable_action_state)
                    .unwrap_or(DurableActionStateProto::DurableActionNone)
                {
                    DurableActionStateProto::DurableActionScheduled => {
                        DurableActionState::Scheduled
                    }
                    DurableActionStateProto::DurableActionInProgress => {
                        DurableActionState::InProgress
                    }
                    DurableActionStateProto::DurableActionCompleted => {
                        DurableActionState::Completed
                    }
                    DurableActionStateProto::DurableActionFailed
                    | DurableActionStateProto::DurableActionNone => DurableActionState::Failed,
                };
                Some(DurableActionObservation {
                    action_id: inner.durable_action_id,
                    signature: inner.durable_action_signature,
                    state,
                    error: (!inner.durable_action_error.is_empty())
                        .then_some(inner.durable_action_error),
                })
            },
            active_replica_connections: inner
                .active_replica_connections
                .into_iter()
                .map(|connection| ReplicaConnectionStatus {
                    id: connection.id,
                    instance_id: ReplicaInstanceId::new(connection.instance_id),
                })
                .collect(),
        })
    }

    async fn execute_durable_action(
        &self,
        action_id: &str,
        action: DurableReplicaAction,
    ) -> Result<()> {
        let action = match action {
            DurableReplicaAction::Open { mode } => {
                execute_durable_action_request::Action::Open(OpenRequest {
                    mode: OpenModeProto::from(mode) as i32,
                })
            }
            DurableReplicaAction::Close => {
                execute_durable_action_request::Action::Close(CloseRequest {})
            }
            DurableReplicaAction::RevokeWriteStatus => {
                execute_durable_action_request::Action::RevokeWriteStatus(
                    RevokeWriteStatusRequest {},
                )
            }
            DurableReplicaAction::ChangeRole { epoch, role } => {
                execute_durable_action_request::Action::ChangeRole(ChangeRoleRequest {
                    epoch: Some(epoch.into()),
                    role: RoleProto::from(role) as i32,
                })
            }
            DurableReplicaAction::UpdateEpoch { epoch } => {
                execute_durable_action_request::Action::UpdateEpoch(UpdateEpochRequest {
                    epoch: Some(epoch.into()),
                })
            }
            DurableReplicaAction::UpdateCatchUpConfiguration { current, previous } => {
                execute_durable_action_request::Action::UpdateCatchUpConfiguration(
                    UpdateCatchUpConfigRequest {
                        current: Some(current.into()),
                        previous: Some(previous.into()),
                    },
                )
            }
            DurableReplicaAction::WaitForCatchUpQuorum { mode } => {
                execute_durable_action_request::Action::WaitForCatchUpQuorum(
                    WaitForCatchUpQuorumRequest {
                        mode: QuorumModeProto::from(mode) as i32,
                    },
                )
            }
            DurableReplicaAction::UpdateCurrentConfiguration { current } => {
                execute_durable_action_request::Action::UpdateCurrentConfiguration(
                    UpdateCurrentConfigRequest {
                        current: Some(current.into()),
                    },
                )
            }
            DurableReplicaAction::BuildReplica { replica } => {
                execute_durable_action_request::Action::BuildReplica(BuildReplicaRequest {
                    replica: Some(replica.into()),
                })
            }
            DurableReplicaAction::RemoveReplica {
                replica_id,
                instance_id,
            } => execute_durable_action_request::Action::RemoveReplica(RemoveReplicaRequest {
                replica_id,
                instance_id: instance_id.to_string(),
            }),
        };
        let mut client = self.client.clone();
        client
            .execute_durable_action(ExecuteDurableActionRequest {
                action_id: action_id.to_string(),
                action: Some(action),
            })
            .await
            .map_err(Self::map_err)?;
        Ok(())
    }
}
