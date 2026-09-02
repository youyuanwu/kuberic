use tokio::sync::{mpsc, oneshot};
use tonic::{Request, Response, Status};

use crate::error::KubericError;
use crate::proto::replicator_control_server::ReplicatorControl;
use crate::proto::*;
use crate::replica_agent::AgentCommand;

/// Control server exposing the single correlated mutation path owned by the
/// pod-local replica agent.
pub struct ControlServer {
    cmd_tx: mpsc::Sender<AgentCommand>,
}

fn runtime_error_status(error: KubericError) -> Status {
    match error {
        KubericError::NoWriteQuorum => Status::unavailable(error.to_string()),
        KubericError::AgentBusy => Status::resource_exhausted(error.to_string()),
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

impl ControlServer {
    pub fn new(cmd_tx: mpsc::Sender<AgentCommand>) -> Self {
        Self { cmd_tx }
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
        let agent = info.agent;
        let current_agent_action = agent
            .current_action
            .map(crate::proto::CorrelatedActionObservationProto::try_from)
            .transpose()
            .map_err(Status::failed_precondition)?;
        let retained_terminal_actions = agent
            .retained_terminal_actions
            .into_iter()
            .map(crate::proto::CorrelatedActionObservationProto::try_from)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Status::failed_precondition)?;
        Ok(Response::new(GetStatusResponse {
            role: crate::proto::RoleProto::from(info.role) as i32,
            epoch: Some(info.epoch.into()),
            current_progress: info.current_progress,
            catch_up_capability: info.catch_up_capability,
            committed_lsn: info.committed_lsn,
            healthy: info.healthy,
            instance_id: info.instance_id.to_string(),
            write_status: Some(crate::proto::AccessStatusProto::from(info.write_status) as i32),
            configuration: info.configuration.map(Into::into),
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
            replica_agent_protocol_version: agent.protocol_version,
            agent_generation: agent.generation.to_string(),
            agent_control_version: agent.control_version.value(),
            current_agent_action,
            retained_terminal_actions,
            local_faults: agent.local_faults.into_iter().map(Into::into).collect(),
            build_observation: info.build_observation.map(Into::into),
            replica_lifecycle_peer_protocol_version: agent.lifecycle_peer_protocol_version,
        }))
    }

    async fn execute_correlated_control_action(
        &self,
        req: Request<ExecuteCorrelatedControlActionRequest>,
    ) -> Result<Response<ExecuteCorrelatedControlActionResponse>, Status> {
        let request = crate::types::CorrelatedControlActionRequest::try_from(req.into_inner())
            .map_err(Status::invalid_argument)?;
        let acknowledgement = self
            .send_cmd(|reply| AgentCommand::ExecuteCorrelatedControlAction {
                request: Box::new(request),
                reply,
            })
            .await?;
        Ok(Response::new(ExecuteCorrelatedControlActionResponse {
            observation: Some(
                crate::proto::CorrelatedActionObservationProto::try_from(
                    acknowledgement.observation,
                )
                .map_err(Status::failed_precondition)?,
            ),
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
