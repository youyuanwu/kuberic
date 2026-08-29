use tokio::sync::{mpsc, oneshot};
use tonic::{Request, Response, Status};

use crate::error::KubericError;
use crate::pod::RuntimeCommand;
use crate::proto::replicator_control_server::ReplicatorControl;
use crate::proto::*;
use crate::types::{DurableReplicaAction, Epoch, ReplicaInstanceId, Role};

/// Control server that routes all commands through the PodRuntime's
/// command channel. This ensures correct replicator/event ordering
/// (e.g., replicator setup before user notification on promotion).
pub struct ControlServer {
    cmd_tx: mpsc::Sender<RuntimeCommand>,
    #[allow(dead_code)]
    replica_id: i64,
}

fn runtime_error_status(error: KubericError) -> Status {
    match error {
        KubericError::NoWriteQuorum => Status::unavailable(error.to_string()),
        other => Status::internal(other.to_string()),
    }
}

impl ControlServer {
    pub fn new(replica_id: i64, cmd_tx: mpsc::Sender<RuntimeCommand>) -> Self {
        Self { cmd_tx, replica_id }
    }

    async fn send_cmd<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<crate::Result<T>>) -> RuntimeCommand,
    ) -> Result<T, Status> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(make(tx))
            .await
            .map_err(|_| Status::unavailable("runtime closed"))?;
        rx.await
            .map_err(|_| Status::unavailable("runtime closed"))?
            .map_err(runtime_error_status)
    }
}

#[tonic::async_trait]
impl ReplicatorControl for ControlServer {
    async fn open(&self, req: Request<OpenRequest>) -> Result<Response<OpenResponse>, Status> {
        let mode = crate::types::OpenMode::from(req.into_inner().mode);
        self.send_cmd(|reply| RuntimeCommand::Open { mode, reply })
            .await?;
        Ok(Response::new(OpenResponse {}))
    }

    async fn close(&self, _req: Request<CloseRequest>) -> Result<Response<CloseResponse>, Status> {
        self.send_cmd(|reply| RuntimeCommand::Close { reply })
            .await?;
        Ok(Response::new(CloseResponse {}))
    }

    async fn change_role(
        &self,
        req: Request<ChangeRoleRequest>,
    ) -> Result<Response<ChangeRoleResponse>, Status> {
        let inner = req.into_inner();
        let epoch: Epoch = inner.epoch.unwrap_or_default().into();
        let role: Role = Role::from(inner.role);
        self.send_cmd(|reply| RuntimeCommand::ChangeRole { epoch, role, reply })
            .await?;
        Ok(Response::new(ChangeRoleResponse {}))
    }

    async fn update_epoch(
        &self,
        req: Request<UpdateEpochRequest>,
    ) -> Result<Response<UpdateEpochResponse>, Status> {
        let epoch: Epoch = req.into_inner().epoch.unwrap_or_default().into();
        self.send_cmd(|reply| RuntimeCommand::UpdateEpoch { epoch, reply })
            .await?;
        Ok(Response::new(UpdateEpochResponse {}))
    }

    async fn get_status(
        &self,
        _req: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(RuntimeCommand::GetStatus { reply: tx })
            .await
            .map_err(|_| Status::unavailable("runtime closed"))?;
        let info = rx
            .await
            .map_err(|_| Status::unavailable("runtime closed"))?;
        Ok(Response::new(GetStatusResponse {
            role: crate::proto::RoleProto::from(info.role) as i32,
            epoch: Some(info.epoch.into()),
            current_progress: info.current_progress,
            catch_up_capability: info.catch_up_capability,
            committed_lsn: info.committed_lsn,
            healthy: info.healthy,
            instance_id: info.instance_id.to_string(),
            write_status: crate::proto::AccessStatusProto::from(info.write_status) as i32,
            configuration: info.configuration.map(Into::into),
            last_completed_action_id: info
                .last_completed_action
                .as_ref()
                .map(|action| action.action_id.clone())
                .unwrap_or_default(),
            last_completed_action_signature: info
                .last_completed_action
                .map(|action| action.signature)
                .unwrap_or_default(),
        }))
    }

    async fn update_catch_up_configuration(
        &self,
        req: Request<UpdateCatchUpConfigRequest>,
    ) -> Result<Response<UpdateCatchUpConfigResponse>, Status> {
        let inner = req.into_inner();
        self.send_cmd(|reply| RuntimeCommand::UpdateCatchUpConfiguration {
            current: inner.current.unwrap_or_default().into(),
            previous: inner.previous.unwrap_or_default().into(),
            reply,
        })
        .await?;
        Ok(Response::new(UpdateCatchUpConfigResponse {}))
    }

    async fn update_current_configuration(
        &self,
        req: Request<UpdateCurrentConfigRequest>,
    ) -> Result<Response<UpdateCurrentConfigResponse>, Status> {
        let inner = req.into_inner();
        self.send_cmd(|reply| RuntimeCommand::UpdateCurrentConfiguration {
            current: inner.current.unwrap_or_default().into(),
            reply,
        })
        .await?;
        Ok(Response::new(UpdateCurrentConfigResponse {}))
    }

    async fn wait_for_catch_up_quorum(
        &self,
        req: Request<WaitForCatchUpQuorumRequest>,
    ) -> Result<Response<WaitForCatchUpQuorumResponse>, Status> {
        let mode = crate::types::ReplicaSetQuorumMode::from(req.into_inner().mode);
        self.send_cmd(|reply| RuntimeCommand::WaitForCatchUpQuorum { mode, reply })
            .await?;
        Ok(Response::new(WaitForCatchUpQuorumResponse {}))
    }

    async fn build_replica(
        &self,
        req: Request<BuildReplicaRequest>,
    ) -> Result<Response<BuildReplicaResponse>, Status> {
        let replica = req
            .into_inner()
            .replica
            .ok_or_else(|| Status::invalid_argument("missing replica"))?
            .into();
        self.send_cmd(|reply| RuntimeCommand::BuildReplica { replica, reply })
            .await?;
        Ok(Response::new(BuildReplicaResponse {}))
    }

    async fn remove_replica(
        &self,
        req: Request<RemoveReplicaRequest>,
    ) -> Result<Response<RemoveReplicaResponse>, Status> {
        let inner = req.into_inner();
        self.send_cmd(|reply| RuntimeCommand::RemoveReplica {
            replica_id: inner.replica_id,
            instance_id: ReplicaInstanceId::new(inner.instance_id),
            reply,
        })
        .await?;
        Ok(Response::new(RemoveReplicaResponse {}))
    }

    async fn on_data_loss(
        &self,
        _req: Request<OnDataLossRequest>,
    ) -> Result<Response<OnDataLossResponse>, Status> {
        let action = self
            .send_cmd(|reply| RuntimeCommand::OnDataLoss { reply })
            .await?;
        Ok(Response::new(OnDataLossResponse {
            state_changed: action == crate::types::DataLossAction::StateChanged,
        }))
    }

    async fn revoke_write_status(
        &self,
        _req: Request<RevokeWriteStatusRequest>,
    ) -> Result<Response<RevokeWriteStatusResponse>, Status> {
        self.send_cmd(|reply| RuntimeCommand::RevokeWriteStatus { reply })
            .await?;
        Ok(Response::new(RevokeWriteStatusResponse {}))
    }

    async fn execute_durable_action(
        &self,
        req: Request<ExecuteDurableActionRequest>,
    ) -> Result<Response<ExecuteDurableActionResponse>, Status> {
        let inner = req.into_inner();
        let action = match inner.action {
            Some(execute_durable_action_request::Action::RevokeWriteStatus(_)) => {
                DurableReplicaAction::RevokeWriteStatus
            }
            Some(execute_durable_action_request::Action::ChangeRole(request)) => {
                DurableReplicaAction::ChangeRole {
                    epoch: request.epoch.unwrap_or_default().into(),
                    role: Role::from(request.role),
                }
            }
            Some(execute_durable_action_request::Action::UpdateEpoch(request)) => {
                DurableReplicaAction::UpdateEpoch {
                    epoch: request.epoch.unwrap_or_default().into(),
                }
            }
            Some(execute_durable_action_request::Action::UpdateCatchUpConfiguration(request)) => {
                DurableReplicaAction::UpdateCatchUpConfiguration {
                    current: request.current.unwrap_or_default().into(),
                    previous: request.previous.unwrap_or_default().into(),
                }
            }
            Some(execute_durable_action_request::Action::WaitForCatchUpQuorum(request)) => {
                DurableReplicaAction::WaitForCatchUpQuorum {
                    mode: crate::types::ReplicaSetQuorumMode::from(request.mode),
                }
            }
            Some(execute_durable_action_request::Action::UpdateCurrentConfiguration(request)) => {
                DurableReplicaAction::UpdateCurrentConfiguration {
                    current: request.current.unwrap_or_default().into(),
                }
            }
            None => return Err(Status::invalid_argument("missing durable action")),
        };
        self.send_cmd(|reply| RuntimeCommand::ExecuteDurableAction {
            action_id: inner.action_id,
            action,
            reply,
        })
        .await?;
        Ok(Response::new(ExecuteDurableActionResponse {}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_write_quorum_is_preserved_as_unavailable() {
        let status = runtime_error_status(KubericError::NoWriteQuorum);
        assert_eq!(status.code(), tonic::Code::Unavailable);
        assert_eq!(status.message(), "no write quorum");
    }

    #[test]
    fn unrelated_errors_keep_existing_internal_mapping() {
        let status = runtime_error_status(KubericError::NotPrimary);
        assert_eq!(status.code(), tonic::Code::Internal);
        assert_eq!(status.message(), "not primary");
    }
}
