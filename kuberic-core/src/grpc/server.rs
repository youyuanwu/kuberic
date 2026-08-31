use tokio::sync::{mpsc, oneshot};
use tonic::{Request, Response, Status};

use crate::error::KubericError;
use crate::proto::replicator_control_server::ReplicatorControl;
use crate::proto::*;
use crate::replica_agent::AgentCommand;
use crate::types::{DurableActionResult, DurableReplicaAction, Epoch, ReplicaInstanceId, Role};

/// Control server that routes all commands through the pod-local replica
/// agent. The agent is the only control-plane path to PodRuntime.
pub struct ControlServer {
    cmd_tx: mpsc::Sender<AgentCommand>,
    #[allow(dead_code)]
    replica_id: i64,
}

fn runtime_error_status(error: KubericError) -> Status {
    match error {
        KubericError::NoWriteQuorum => Status::unavailable(error.to_string()),
        KubericError::AgentBusy | KubericError::AgentQueueFull => {
            Status::resource_exhausted(error.to_string())
        }
        KubericError::ActionIdConflict { .. } => Status::already_exists(error.to_string()),
        KubericError::ActionSignatureMismatch { .. } | KubericError::InvalidCorrelatedActionId => {
            Status::invalid_argument(error.to_string())
        }
        KubericError::UnsupportedControlProtocolVersion { .. } => {
            Status::unimplemented(error.to_string())
        }
        KubericError::CorrelatedTargetMismatch { .. }
        | KubericError::StaleAgentGeneration { .. }
        | KubericError::StaleAgentControlVersion { .. }
        | KubericError::StaleEpoch { .. } => Status::failed_precondition(error.to_string()),
        KubericError::CorrelatedContinuityUnavailable { .. } => Status::aborted(error.to_string()),
        other => Status::internal(other.to_string()),
    }
}

fn action_result_proto(result: Option<DurableActionResult>) -> i32 {
    result
        .map(crate::proto::DurableActionResultProto::from)
        .unwrap_or(crate::proto::DurableActionResultProto::DurableActionResultNone) as i32
}

impl ControlServer {
    pub fn new(replica_id: i64, cmd_tx: mpsc::Sender<AgentCommand>) -> Self {
        Self { cmd_tx, replica_id }
    }

    async fn send_cmd<T>(
        &self,
        make: impl FnOnce(oneshot::Sender<crate::Result<T>>) -> AgentCommand,
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
        self.send_cmd(|reply| AgentCommand::Open { mode, reply })
            .await?;
        Ok(Response::new(OpenResponse {}))
    }

    async fn close(&self, _req: Request<CloseRequest>) -> Result<Response<CloseResponse>, Status> {
        self.send_cmd(|reply| AgentCommand::Close { reply }).await?;
        Ok(Response::new(CloseResponse {}))
    }

    async fn change_role(
        &self,
        req: Request<ChangeRoleRequest>,
    ) -> Result<Response<ChangeRoleResponse>, Status> {
        let inner = req.into_inner();
        let epoch: Epoch = inner.epoch.unwrap_or_default().into();
        let role: Role = Role::from(inner.role);
        self.send_cmd(|reply| AgentCommand::ChangeRole { epoch, role, reply })
            .await?;
        Ok(Response::new(ChangeRoleResponse {}))
    }

    async fn update_epoch(
        &self,
        req: Request<UpdateEpochRequest>,
    ) -> Result<Response<UpdateEpochResponse>, Status> {
        let epoch: Epoch = req.into_inner().epoch.unwrap_or_default().into();
        self.send_cmd(|reply| AgentCommand::UpdateEpoch { epoch, reply })
            .await?;
        Ok(Response::new(UpdateEpochResponse {}))
    }

    async fn get_status(
        &self,
        _req: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx
            .send(AgentCommand::GetStatus { reply: tx })
            .await
            .map_err(|_| Status::unavailable("runtime closed"))?;
        let info = rx
            .await
            .map_err(|_| Status::unavailable("runtime closed"))?;
        let last_completed_action_result = action_result_proto(
            info.last_completed_action
                .as_ref()
                .and_then(|action| action.result),
        );
        let durable_action_result = action_result_proto(
            info.durable_action
                .as_ref()
                .and_then(|action| action.result),
        );
        let (
            replica_agent_protocol_version,
            agent_generation,
            agent_control_version,
            current_agent_action,
            retained_terminal_actions,
            local_faults,
        ) = info.agent.as_ref().map_or_else(
            || (0, String::new(), 0, None, Vec::new(), Vec::new()),
            |agent| {
                if !agent
                    .capabilities
                    .contains(&crate::types::ReplicaAgentCapability::CorrelatedControlActionV1)
                {
                    return (0, String::new(), 0, None, Vec::new(), Vec::new());
                }
                (
                    crate::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION,
                    agent.generation.to_string(),
                    agent.control_version.value(),
                    agent.current_action.clone().map(Into::into),
                    agent
                        .retained_terminal_actions
                        .iter()
                        .cloned()
                        .map(Into::into)
                        .collect(),
                    agent.local_faults.iter().copied().map(Into::into).collect(),
                )
            },
        );
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
            durable_action_id: info
                .durable_action
                .as_ref()
                .map(|action| action.action_id.clone())
                .unwrap_or_default(),
            durable_action_signature: info
                .durable_action
                .as_ref()
                .map(|action| action.signature.clone())
                .unwrap_or_default(),
            durable_action_state: info
                .durable_action
                .as_ref()
                .map(|action| match action.state {
                    crate::types::DurableActionState::Scheduled => {
                        crate::proto::DurableActionStateProto::DurableActionScheduled
                    }
                    crate::types::DurableActionState::InProgress => {
                        crate::proto::DurableActionStateProto::DurableActionInProgress
                    }
                    crate::types::DurableActionState::Completed => {
                        crate::proto::DurableActionStateProto::DurableActionCompleted
                    }
                    crate::types::DurableActionState::Failed => {
                        crate::proto::DurableActionStateProto::DurableActionFailed
                    }
                } as i32)
                .unwrap_or(crate::proto::DurableActionStateProto::DurableActionNone as i32),
            durable_action_error: info
                .durable_action
                .and_then(|action| action.error)
                .unwrap_or_default(),
            active_replica_connections: info
                .active_replica_connections
                .into_iter()
                .map(|connection| ReplicaConnectionStatusProto {
                    id: connection.id,
                    instance_id: connection.instance_id.to_string(),
                })
                .collect(),
            deactivation_info: info.deactivation_info.map(Into::into),
            election_configuration: info.election_configuration.map(Into::into),
            last_completed_action_result,
            durable_action_result,
            replica_agent_protocol_version,
            agent_generation,
            agent_control_version,
            current_agent_action,
            retained_terminal_actions,
            local_faults,
        }))
    }

    async fn update_catch_up_configuration(
        &self,
        req: Request<UpdateCatchUpConfigRequest>,
    ) -> Result<Response<UpdateCatchUpConfigResponse>, Status> {
        let inner = req.into_inner();
        self.send_cmd(|reply| AgentCommand::UpdateCatchUpConfiguration {
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
        self.send_cmd(|reply| AgentCommand::UpdateCurrentConfiguration {
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
        self.send_cmd(|reply| AgentCommand::WaitForCatchUpQuorum { mode, reply })
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
        self.send_cmd(|reply| AgentCommand::BuildReplica { replica, reply })
            .await?;
        Ok(Response::new(BuildReplicaResponse {}))
    }

    async fn remove_replica(
        &self,
        req: Request<RemoveReplicaRequest>,
    ) -> Result<Response<RemoveReplicaResponse>, Status> {
        let inner = req.into_inner();
        self.send_cmd(|reply| AgentCommand::RemoveReplica {
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
            .send_cmd(|reply| AgentCommand::OnDataLoss { reply })
            .await?;
        Ok(Response::new(OnDataLossResponse {
            state_changed: action == crate::types::DataLossAction::StateChanged,
        }))
    }

    async fn revoke_write_status(
        &self,
        _req: Request<RevokeWriteStatusRequest>,
    ) -> Result<Response<RevokeWriteStatusResponse>, Status> {
        self.send_cmd(|reply| AgentCommand::RevokeWriteStatus { reply })
            .await?;
        Ok(Response::new(RevokeWriteStatusResponse {}))
    }

    async fn execute_durable_action(
        &self,
        req: Request<ExecuteDurableActionRequest>,
    ) -> Result<Response<ExecuteDurableActionResponse>, Status> {
        let inner = req.into_inner();
        if inner.action_id.is_empty() {
            return Err(Status::invalid_argument(
                KubericError::InvalidCorrelatedActionId.to_string(),
            ));
        }
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
            Some(execute_durable_action_request::Action::Open(request)) => {
                DurableReplicaAction::Open {
                    mode: crate::types::OpenMode::from(request.mode),
                }
            }
            Some(execute_durable_action_request::Action::Close(_)) => DurableReplicaAction::Close,
            Some(execute_durable_action_request::Action::BuildReplica(request)) => {
                DurableReplicaAction::BuildReplica {
                    replica: request.replica.unwrap_or_default().into(),
                }
            }
            Some(execute_durable_action_request::Action::RemoveReplica(request)) => {
                DurableReplicaAction::RemoveReplica {
                    replica_id: request.replica_id,
                    instance_id: crate::types::ReplicaInstanceId::new(request.instance_id),
                }
            }
            Some(execute_durable_action_request::Action::OnDataLoss(request)) => {
                DurableReplicaAction::OnDataLoss {
                    epoch: request
                        .expected_epoch
                        .ok_or_else(|| Status::invalid_argument("missing data-loss epoch"))?
                        .into(),
                }
            }
            Some(execute_durable_action_request::Action::RecordElectionConfiguration(request)) => {
                let configuration = request
                    .configuration
                    .ok_or_else(|| Status::invalid_argument("missing election configuration"))?;
                DurableReplicaAction::RecordElectionConfiguration {
                    configuration: crate::types::ReplicaElectionConfiguration::try_from(
                        configuration,
                    )
                    .map_err(Status::invalid_argument)?,
                }
            }
            None => return Err(Status::invalid_argument("missing durable action")),
        };
        self.send_cmd(|reply| AgentCommand::ExecuteDurableAction {
            action_id: inner.action_id,
            action,
            reply,
        })
        .await?;
        Ok(Response::new(ExecuteDurableActionResponse {}))
    }

    async fn execute_correlated_control_action(
        &self,
        req: Request<ExecuteCorrelatedControlActionRequest>,
    ) -> Result<Response<ExecuteCorrelatedControlActionResponse>, Status> {
        let request = crate::types::CorrelatedControlActionRequest::try_from(req.into_inner())
            .map_err(Status::invalid_argument)?;
        let acknowledgement = self
            .send_cmd(|reply| AgentCommand::ExecuteCorrelatedControlAction { request, reply })
            .await?;
        Ok(Response::new(ExecuteCorrelatedControlActionResponse {
            observation: Some(acknowledgement.observation.into()),
        }))
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

    #[test]
    fn replica_agent_errors_keep_distinct_transport_classes() {
        assert_eq!(
            runtime_error_status(KubericError::AgentBusy).code(),
            tonic::Code::ResourceExhausted
        );
        assert_eq!(
            runtime_error_status(KubericError::ActionIdConflict {
                action_id: "action".to_string(),
            })
            .code(),
            tonic::Code::AlreadyExists
        );
        assert_eq!(
            runtime_error_status(KubericError::UnsupportedControlProtocolVersion { got: 2 }).code(),
            tonic::Code::Unimplemented
        );
        assert_eq!(
            runtime_error_status(KubericError::CorrelatedContinuityUnavailable {
                action_id: "action".to_string(),
            })
            .code(),
            tonic::Code::Aborted
        );
    }
}
