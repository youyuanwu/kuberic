use tonic::transport::Channel;

use crate::add_replica::{
    PEER_TERMINAL_RETENTION, PeerAddBuildStatus, PeerStageObservation, PeerStageRequest,
    REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION,
};
use crate::error::{KubericError, Result};
use crate::proto::replica_add_build_peer_client::ReplicaAddBuildPeerClient;
use crate::proto::{ExecuteAddBuildStageRequest, GetAddBuildStatusRequest};
use crate::types::{AgentGeneration, ReplicaId, ReplicaInstanceId};

#[derive(Clone)]
pub struct GrpcPeerClient {
    client: ReplicaAddBuildPeerClient<Channel>,
    target_replica_id: ReplicaId,
    target_instance_id: ReplicaInstanceId,
    target_generation: AgentGeneration,
}

impl GrpcPeerClient {
    pub async fn connect(
        address: String,
        target_replica_id: ReplicaId,
        target_instance_id: ReplicaInstanceId,
        target_generation: AgentGeneration,
    ) -> Result<Self> {
        let channel = Channel::from_shared(address)
            .map_err(|error| KubericError::Internal(Box::new(error)))?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await
            .map_err(|error| KubericError::RemotePeerUnavailable(error.to_string()))?;
        Ok(Self {
            client: ReplicaAddBuildPeerClient::new(channel),
            target_replica_id,
            target_instance_id,
            target_generation,
        })
    }

    fn map_err(error: tonic::Status) -> KubericError {
        match error.code() {
            tonic::Code::Unimplemented => {
                KubericError::RemoteControlProtocolUnsupported(error.message().to_string())
            }
            tonic::Code::AlreadyExists => {
                KubericError::RemoteAgentConflict(error.message().to_string())
            }
            tonic::Code::FailedPrecondition | tonic::Code::InvalidArgument => {
                KubericError::RemotePeerRequestRejected(error.message().to_string())
            }
            tonic::Code::ResourceExhausted => KubericError::AgentBusy,
            tonic::Code::Unavailable => {
                KubericError::RemotePeerUnavailable(error.message().to_string())
            }
            _ => KubericError::Internal(Box::new(error)),
        }
    }

    pub async fn get_status(&self, timeout: std::time::Duration) -> Result<PeerAddBuildStatus> {
        let mut client = self.client.clone();
        let response = tokio::time::timeout(
            timeout,
            client.get_add_build_status(GetAddBuildStatusRequest {
                protocol_version: REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION,
                target_replica_id: self.target_replica_id,
                target_instance_id: self.target_instance_id.to_string(),
                expected_target_agent_generation: self.target_generation.to_string(),
            }),
        )
        .await
        .map_err(|_| KubericError::RemotePeerUnavailable("peer status timed out".to_string()))?
        .map_err(Self::map_err)?
        .into_inner();
        let status = PeerAddBuildStatus::try_from(response)
            .map_err(KubericError::RemotePeerRequestRejected)?;
        if status.protocol_version != REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION
            || status.target_replica_id != self.target_replica_id
            || status.target_instance_id != self.target_instance_id
            || status.agent_generation != self.target_generation
            || status.retained_terminal_actions.len() > PEER_TERMINAL_RETENTION
        {
            return Err(KubericError::RemotePeerRequestRejected(
                "peer status does not match the expected target".to_string(),
            ));
        }
        Ok(status)
    }

    pub async fn execute_stage(
        &self,
        request: PeerStageRequest,
        timeout: std::time::Duration,
    ) -> Result<PeerStageObservation> {
        let expected_id = request.message_id.clone();
        let expected_signature = request.input_signature.clone();
        let expected_stage = request.stage;
        let mut client = self.client.clone();
        let response = tokio::time::timeout(
            timeout,
            client.execute_add_build_stage(ExecuteAddBuildStageRequest::from(request)),
        )
        .await
        .map_err(|_| KubericError::RemotePeerUnavailable("peer stage timed out".to_string()))?
        .map_err(Self::map_err)?
        .into_inner();
        let observation =
            PeerStageObservation::try_from(response.observation.ok_or_else(|| {
                KubericError::RemotePeerRequestRejected(
                    "peer stage response has no observation".to_string(),
                )
            })?)
            .map_err(KubericError::RemotePeerRequestRejected)?;
        if observation.protocol_version != REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION
            || observation.message_id != expected_id
            || observation.input_signature != expected_signature
            || observation.stage != expected_stage
            || observation.target_agent_generation != self.target_generation
        {
            return Err(KubericError::RemotePeerRequestRejected(
                "peer stage response does not match the request".to_string(),
            ));
        }
        Ok(observation)
    }
}
