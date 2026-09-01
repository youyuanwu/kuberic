use tokio::sync::{mpsc, oneshot};
use tonic::{Request, Response, Status};

use crate::error::KubericError;
use crate::proto::replica_add_build_peer_server::ReplicaAddBuildPeer;
use crate::proto::{
    ExecuteAddBuildStageRequest, ExecuteAddBuildStageResponse, GetAddBuildStatusRequest,
    GetAddBuildStatusResponse,
};
use crate::replica_agent::AgentCommand;
use crate::types::{AgentGeneration, ReplicaInstanceId};

pub struct PeerServer {
    cmd_tx: mpsc::Sender<AgentCommand>,
}

impl PeerServer {
    pub fn new(cmd_tx: mpsc::Sender<AgentCommand>) -> Self {
        Self { cmd_tx }
    }
}

fn peer_error_status(error: KubericError) -> Status {
    match error {
        KubericError::UnsupportedPeerProtocolVersion { .. } => {
            Status::unimplemented(error.to_string())
        }
        KubericError::PeerStageIdConflict { .. } => Status::already_exists(error.to_string()),
        KubericError::PeerStageTargetMismatch(_)
        | KubericError::PeerStageStale(_)
        | KubericError::StaleAgentGeneration { .. }
        | KubericError::StaleEpoch { .. } => Status::failed_precondition(error.to_string()),
        KubericError::ActionSignatureMismatch { .. } => Status::invalid_argument(error.to_string()),
        KubericError::AgentBusy => Status::resource_exhausted(error.to_string()),
        KubericError::Closed => Status::unavailable(error.to_string()),
        other => Status::internal(other.to_string()),
    }
}

#[tonic::async_trait]
impl ReplicaAddBuildPeer for PeerServer {
    async fn get_add_build_status(
        &self,
        request: Request<GetAddBuildStatusRequest>,
    ) -> Result<Response<GetAddBuildStatusResponse>, Status> {
        let request = request.into_inner();
        if request.protocol_version != crate::add_replica::REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION {
            return Err(Status::unimplemented(format!(
                "unsupported replica add/build peer protocol version {}",
                request.protocol_version
            )));
        }
        let generation = AgentGeneration::parse(request.expected_target_agent_generation)
            .map_err(Status::invalid_argument)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(AgentCommand::GetAddBuildStatus {
                target_replica_id: request.target_replica_id,
                target_instance_id: ReplicaInstanceId::new(request.target_instance_id),
                expected_generation: generation,
                reply: reply_tx,
            })
            .await
            .map_err(|_| Status::unavailable("replica agent closed"))?;
        let status = reply_rx
            .await
            .map_err(|_| Status::unavailable("replica agent closed"))?
            .map_err(peer_error_status)?;
        Ok(Response::new(status.into()))
    }

    async fn execute_add_build_stage(
        &self,
        request: Request<ExecuteAddBuildStageRequest>,
    ) -> Result<Response<ExecuteAddBuildStageResponse>, Status> {
        let request = crate::add_replica::PeerStageRequest::try_from(request.into_inner())
            .map_err(Status::invalid_argument)?;
        let (reply_tx, reply_rx) = oneshot::channel();
        self.cmd_tx
            .send(AgentCommand::ExecuteAddBuildStage {
                request: Box::new(request),
                reply: reply_tx,
            })
            .await
            .map_err(|_| Status::unavailable("replica agent closed"))?;
        let observation = reply_rx
            .await
            .map_err(|_| Status::unavailable("replica agent closed"))?
            .map_err(peer_error_status)?;
        Ok(Response::new(ExecuteAddBuildStageResponse {
            observation: Some(observation.into()),
        }))
    }
}
