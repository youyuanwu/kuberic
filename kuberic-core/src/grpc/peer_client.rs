use tonic::transport::Channel;

use crate::error::{KubericError, Result};
use crate::proto::replica_lifecycle_peer_client::ReplicaLifecyclePeerClient;
use crate::proto::{ExecuteLifecycleStageRequest, GetLifecycleStatusRequest};
use crate::replica_lifecycle::{
    PEER_TERMINAL_RETENTION, PeerLifecycleStatus, PeerStageObservation, PeerStageRequest,
    REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
};
use crate::types::{AgentGeneration, ReplicaId, ReplicaInstanceId};

#[derive(Clone)]
pub struct GrpcPeerClient {
    client: ReplicaLifecyclePeerClient<Channel>,
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
        Self::connect_with_timeout(
            address,
            target_replica_id,
            target_instance_id,
            target_generation,
            std::time::Duration::from_secs(5),
        )
        .await
    }

    pub async fn connect_with_timeout(
        address: String,
        target_replica_id: ReplicaId,
        target_instance_id: ReplicaInstanceId,
        target_generation: AgentGeneration,
        timeout: std::time::Duration,
    ) -> Result<Self> {
        let connect_timeout = timeout.min(std::time::Duration::from_secs(5));
        if connect_timeout.is_zero() {
            return Err(KubericError::RemotePeerUnavailable(
                "peer connect budget is exhausted".to_string(),
            ));
        }
        let channel = Channel::from_shared(address)
            .map_err(|error| KubericError::Internal(Box::new(error)))?
            .connect_timeout(connect_timeout);
        let channel = tokio::time::timeout(timeout, channel.connect())
            .await
            .map_err(|_| KubericError::RemotePeerUnavailable("peer connect timed out".to_string()))?
            .map_err(|error| KubericError::RemotePeerUnavailable(error.to_string()))?;
        Ok(Self {
            client: ReplicaLifecyclePeerClient::new(channel),
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

    pub async fn get_status(&self, timeout: std::time::Duration) -> Result<PeerLifecycleStatus> {
        let mut client = self.client.clone();
        let response = tokio::time::timeout(
            timeout,
            client.get_lifecycle_status(GetLifecycleStatusRequest {
                protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
                target_replica_id: self.target_replica_id,
                target_instance_id: self.target_instance_id.to_string(),
                expected_target_agent_generation: self.target_generation.to_string(),
            }),
        )
        .await
        .map_err(|_| KubericError::RemotePeerUnavailable("peer status timed out".to_string()))?
        .map_err(Self::map_err)?
        .into_inner();
        let status = PeerLifecycleStatus::try_from(response)
            .map_err(KubericError::RemotePeerRequestRejected)?;
        if status.protocol_version != REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION
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
        request
            .validate()
            .map_err(KubericError::RemotePeerRequestRejected)?;
        let expected_id = request.message_id.clone();
        let expected_signature = request.input_signature.clone();
        let expected_stage = request.stage;
        let expected_operation_kind = request.operation_kind;
        let expected_stage_semantic_version = request.stage_semantic_version;
        let mut client = self.client.clone();
        let response = tokio::time::timeout(
            timeout,
            client.execute_lifecycle_stage(
                ExecuteLifecycleStageRequest::try_from(request)
                    .map_err(KubericError::RemotePeerRequestRejected)?,
            ),
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
        if observation.protocol_version != REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION
            || observation.operation_kind != expected_operation_kind
            || observation.stage_semantic_version != expected_stage_semantic_version
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
