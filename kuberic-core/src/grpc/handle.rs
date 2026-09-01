use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};

use async_trait::async_trait;
use tonic::transport::Channel;

use crate::driver::ReplicaHandle;
use crate::error::{KubericError, Result};
use crate::proto::replicator_control_client::ReplicatorControlClient;
use crate::proto::*;
use crate::types::{
    AgentControlVersion, AgentGeneration, CorrelatedActionObservation,
    CorrelatedControlActionAcknowledgement, CorrelatedControlActionRequest, DurableActionState,
    Lsn, ReplicaAgentStatus, ReplicaConnectionStatus, ReplicaId, ReplicaInstanceId,
    ReplicaStatusInfo, Role,
};

/// Production replica handle. Status and all mutations use the public
/// `ReplicatorControl` service; mutation is available only through the
/// correlated control action.
pub struct GrpcReplicaHandle {
    id: ReplicaId,
    instance_id: ReplicaInstanceId,
    client: ReplicatorControlClient<Channel>,
    control_address: String,
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
        let channel = Channel::from_shared(control_address.clone())
            .map_err(|error| KubericError::Internal(Box::new(error)))?
            .connect_timeout(std::time::Duration::from_secs(5))
            .connect()
            .await
            .map_err(|error| KubericError::Internal(Box::new(error)))?;

        Ok(Self {
            id,
            instance_id,
            client: ReplicatorControlClient::new(channel),
            control_address,
            data_address,
            current_progress: AtomicI64::new(0),
            catch_up_capability: AtomicI64::new(0),
        })
    }

    fn map_err(error: tonic::Status) -> KubericError {
        match error.code() {
            tonic::Code::FailedPrecondition => {
                if error.message().contains("replica-agent")
                    || error.message().contains("stale agent")
                    || error.message().contains("stale epoch")
                    || error.message().contains("correlated action target")
                {
                    KubericError::RemoteAgentPreconditionRejected(error.message().to_string())
                } else {
                    KubericError::NotPrimary
                }
            }
            tonic::Code::ResourceExhausted => KubericError::AgentBusy,
            tonic::Code::AlreadyExists => {
                KubericError::RemoteAgentConflict(error.message().to_string())
            }
            tonic::Code::Aborted => {
                KubericError::RemoteAgentContinuityUnavailable(error.message().to_string())
            }
            tonic::Code::Unimplemented => {
                KubericError::RemoteControlProtocolUnsupported(error.message().to_string())
            }
            tonic::Code::InvalidArgument => {
                KubericError::RemoteAgentRequestRejected(error.message().to_string())
            }
            tonic::Code::Unavailable => {
                if error.message().contains("no write quorum") {
                    KubericError::NoWriteQuorum
                } else if error.message().contains("reconfiguration") {
                    KubericError::ReconfigurationPending
                } else {
                    KubericError::Internal(Box::new(error))
                }
            }
            _ => KubericError::Internal(Box::new(error)),
        }
    }

    fn invalid_status(message: impl Into<String>) -> KubericError {
        KubericError::RemoteAgentRequestRejected(message.into())
    }

    fn validate_protocol_version(version: u32) -> Result<()> {
        if version != crate::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION {
            return Err(KubericError::RemoteControlProtocolUnsupported(format!(
                "replica-agent status protocol version must be {}, got {version}",
                crate::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION,
            )));
        }
        Ok(())
    }

    fn validate_agent_observations(agent: &ReplicaAgentStatus) -> Result<()> {
        if agent.retained_terminal_actions.len() > crate::replica_agent::TERMINAL_RETENTION {
            return Err(Self::invalid_status(format!(
                "replica-agent returned {} retained terminal actions; maximum is {}",
                agent.retained_terminal_actions.len(),
                crate::replica_agent::TERMINAL_RETENTION
            )));
        }
        if agent.local_faults.len() > crate::replica_agent::FAULT_RETENTION {
            return Err(Self::invalid_status(format!(
                "replica-agent returned {} local faults; maximum is {}",
                agent.local_faults.len(),
                crate::replica_agent::FAULT_RETENTION
            )));
        }
        let accepted_terminal_count = agent
            .control_version
            .value()
            .saturating_sub(u64::from(agent.current_action.is_some()));
        let expected_retained =
            accepted_terminal_count.min(crate::replica_agent::TERMINAL_RETENTION as u64);
        if agent.retained_terminal_actions.len() as u64 != expected_retained {
            return Err(Self::invalid_status(format!(
                "replica-agent retained {} terminal actions, expected {expected_retained} at control version {}",
                agent.retained_terminal_actions.len(),
                agent.control_version.value()
            )));
        }

        let mut action_ids = HashSet::new();
        if let Some(current) = &agent.current_action {
            Self::validate_observation(agent, current)?;
            if current.control_version != agent.control_version {
                return Err(Self::invalid_status(
                    "replica-agent current action control version is not current",
                ));
            }
            if !matches!(
                current.action.state,
                DurableActionState::Scheduled | DurableActionState::InProgress
            ) {
                return Err(Self::invalid_status(
                    "replica-agent current action is terminal",
                ));
            }
            action_ids.insert(current.action.action_id.as_str());
        }
        for terminal in &agent.retained_terminal_actions {
            Self::validate_observation(agent, terminal)?;
            if !matches!(
                terminal.action.state,
                DurableActionState::Completed | DurableActionState::Failed
            ) {
                return Err(Self::invalid_status(
                    "replica-agent retained action is not terminal",
                ));
            }
            if !action_ids.insert(terminal.action.action_id.as_str()) {
                return Err(Self::invalid_status(format!(
                    "replica-agent returned duplicate action ID {}",
                    terminal.action.action_id
                )));
            }
        }
        if agent.retained_terminal_actions.windows(2).any(|pair| {
            pair[0].control_version.value().saturating_add(1) != pair[1].control_version.value()
        }) {
            return Err(Self::invalid_status(
                "replica-agent retained actions are not control-version contiguous",
            ));
        }
        let newest_terminal_version = agent
            .retained_terminal_actions
            .last()
            .map(|observation| observation.control_version);
        match (&agent.current_action, newest_terminal_version) {
            (Some(current), Some(terminal))
                if terminal.value().saturating_add(1) != current.control_version.value() =>
            {
                return Err(Self::invalid_status(
                    "replica-agent current action does not follow retained history",
                ));
            }
            (None, Some(terminal)) if terminal != agent.control_version => {
                return Err(Self::invalid_status(
                    "replica-agent newest terminal is not at the current control version",
                ));
            }
            (None, None) if agent.control_version.value() != 0 => {
                return Err(Self::invalid_status(
                    "replica-agent has a nonzero control version without action history",
                ));
            }
            _ => {}
        }
        if agent
            .local_faults
            .windows(2)
            .any(|pair| pair[0].sequence >= pair[1].sequence)
            || agent
                .local_faults
                .first()
                .is_some_and(|fault| fault.sequence == 0)
        {
            return Err(Self::invalid_status(
                "replica-agent local faults are not in sequence order",
            ));
        }
        Ok(())
    }

    fn validate_observation(
        agent: &ReplicaAgentStatus,
        observation: &CorrelatedActionObservation,
    ) -> Result<()> {
        if observation.generation != agent.generation {
            return Err(Self::invalid_status(format!(
                "action {} belongs to another agent generation",
                observation.action.action_id
            )));
        }
        if observation.control_version > agent.control_version {
            return Err(Self::invalid_status(format!(
                "action {} has control version beyond agent status",
                observation.action.action_id
            )));
        }
        if observation.control_version.value() == 0 {
            return Err(Self::invalid_status(format!(
                "action {} has zero control version",
                observation.action.action_id
            )));
        }
        match observation.action.state {
            DurableActionState::Scheduled | DurableActionState::InProgress
                if observation.action.error_class.is_some()
                    || observation.action.error.is_some()
                    || observation.action.result.is_some() =>
            {
                return Err(Self::invalid_status(format!(
                    "non-terminal action {} carries a terminal outcome",
                    observation.action.action_id
                )));
            }
            DurableActionState::Completed
                if observation.action.error_class.is_some()
                    || observation.action.error.is_some() =>
            {
                return Err(Self::invalid_status(format!(
                    "completed action {} carries an error",
                    observation.action.action_id
                )));
            }
            DurableActionState::Failed
                if observation.action.error_class.is_none()
                    || observation.action.error.is_none()
                    || observation.action.result.is_some() =>
            {
                return Err(Self::invalid_status(format!(
                    "failed action {} has a malformed outcome",
                    observation.action.action_id
                )));
            }
            _ => {}
        }
        Ok(())
    }

    fn decode_runtime_configuration(
        configuration: Option<ReplicaConfigurationStatusProto>,
    ) -> Result<Option<crate::types::ReplicaConfigurationStatus>> {
        configuration
            .map(crate::grpc::convert::try_runtime_configuration_status)
            .transpose()
            .map_err(Self::invalid_status)
    }

    fn decode_write_status(value: Option<i32>) -> Result<crate::types::AccessStatus> {
        crate::grpc::convert::try_access_status(
            value.ok_or_else(|| Self::invalid_status("missing runtime write status"))?,
        )
        .map_err(Self::invalid_status)
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

    fn current_progress(&self) -> Lsn {
        self.current_progress.load(Ordering::Acquire)
    }

    fn catch_up_capability(&self) -> Lsn {
        self.catch_up_capability.load(Ordering::Acquire)
    }

    fn replicator_address(&self) -> String {
        self.data_address.clone()
    }

    fn control_address(&self) -> String {
        self.control_address.clone()
    }

    async fn get_status(&self) -> Result<ReplicaStatusInfo> {
        let mut client = self.client.clone();
        let inner = client
            .get_status(GetStatusRequest {})
            .await
            .map_err(Self::map_err)?
            .into_inner();

        Self::validate_protocol_version(inner.replica_agent_protocol_version)?;
        if inner.replica_add_build_peer_protocol_version
            != crate::add_replica::REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION
        {
            return Err(KubericError::RemoteControlProtocolUnsupported(format!(
                "replica add/build peer protocol version must be {}, got {}",
                crate::add_replica::REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION,
                inner.replica_add_build_peer_protocol_version
            )));
        }
        if inner.instance_id.is_empty() {
            return Err(Self::invalid_status("missing runtime replica incarnation"));
        }
        let epoch = inner
            .epoch
            .map(crate::types::Epoch::from)
            .ok_or_else(|| Self::invalid_status("missing runtime epoch"))?;
        let role_proto = RoleProto::try_from(inner.role)
            .map_err(|_| Self::invalid_status(format!("unknown runtime role {}", inner.role)))?;
        let generation =
            AgentGeneration::parse(inner.agent_generation).map_err(Self::invalid_status)?;
        let current_action = inner
            .current_agent_action
            .map(CorrelatedActionObservation::try_from)
            .transpose()
            .map_err(Self::invalid_status)?;
        let retained_terminal_actions = inner
            .retained_terminal_actions
            .into_iter()
            .map(CorrelatedActionObservation::try_from)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Self::invalid_status)?;
        let local_faults = inner
            .local_faults
            .into_iter()
            .map(crate::types::LocalFaultRecord::try_from)
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(Self::invalid_status)?;
        let agent = ReplicaAgentStatus {
            protocol_version: inner.replica_agent_protocol_version,
            add_build_peer_protocol_version: inner.replica_add_build_peer_protocol_version,
            generation,
            control_version: AgentControlVersion::new(inner.agent_control_version),
            current_action,
            retained_terminal_actions,
            local_faults,
        };
        Self::validate_agent_observations(&agent)?;

        let election_configuration = inner
            .election_configuration
            .map(crate::types::ReplicaElectionConfiguration::try_from)
            .transpose()
            .map_err(Self::invalid_status)?;
        let deactivation_info = inner
            .deactivation_info
            .map(crate::types::ReplicaDeactivationInfo::try_from)
            .transpose()
            .map_err(Self::invalid_status)?;

        self.current_progress
            .store(inner.current_progress, Ordering::Release);
        self.catch_up_capability.store(
            inner.catch_up_capability.unwrap_or_default(),
            Ordering::Release,
        );

        Ok(ReplicaStatusInfo {
            instance_id: ReplicaInstanceId::new(inner.instance_id),
            role: Role::from(role_proto),
            epoch,
            current_progress: inner.current_progress,
            catch_up_capability: inner.catch_up_capability,
            committed_lsn: inner.committed_lsn,
            healthy: inner.healthy,
            write_status: Self::decode_write_status(inner.write_status)?,
            configuration: Self::decode_runtime_configuration(inner.configuration)?,
            election_configuration,
            deactivation_info,
            active_replica_connections: inner
                .active_replica_connections
                .into_iter()
                .map(|connection| ReplicaConnectionStatus {
                    id: connection.id,
                    instance_id: ReplicaInstanceId::new(connection.instance_id),
                })
                .collect(),
            build_observation: inner
                .build_observation
                .map(crate::add_replica::RuntimeBuildObservation::try_from)
                .transpose()
                .map_err(Self::invalid_status)?,
            agent,
        })
    }

    async fn execute_correlated_control_action(
        &self,
        request: CorrelatedControlActionRequest,
    ) -> Result<CorrelatedControlActionAcknowledgement> {
        let mut client = self.client.clone();
        let response = client
            .execute_correlated_control_action(ExecuteCorrelatedControlActionRequest::from(request))
            .await
            .map_err(Self::map_err)?
            .into_inner();
        let observation = response
            .observation
            .ok_or_else(|| Self::invalid_status("missing correlated control acknowledgement"))?
            .try_into()
            .map_err(Self::invalid_status)?;
        Ok(CorrelatedControlActionAcknowledgement { observation })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{DurableActionObservation, LocalFaultRecord, ReplicaAgentStatus};

    fn observation(
        generation: &AgentGeneration,
        id: &str,
        version: u64,
        state: DurableActionState,
    ) -> CorrelatedActionObservation {
        CorrelatedActionObservation {
            generation: generation.clone(),
            control_version: AgentControlVersion::new(version),
            action: DurableActionObservation {
                action_id: id.to_string(),
                signature: format!("signature-{id}"),
                state,
                error_class: None,
                error: None,
                result: None,
                add_replica_progress: None,
            },
        }
    }

    fn agent_status() -> ReplicaAgentStatus {
        let generation = AgentGeneration::from_string("generation");
        ReplicaAgentStatus {
            protocol_version: crate::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION,
            add_build_peer_protocol_version:
                crate::add_replica::REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION,
            generation: generation.clone(),
            control_version: AgentControlVersion::new(2),
            current_action: Some(observation(
                &generation,
                "current",
                2,
                DurableActionState::InProgress,
            )),
            retained_terminal_actions: vec![observation(
                &generation,
                "terminal",
                1,
                DurableActionState::Completed,
            )],
            local_faults: Vec::<LocalFaultRecord>::new(),
        }
    }

    #[test]
    fn strict_agent_status_accepts_well_formed_ledger() {
        GrpcReplicaHandle::validate_agent_observations(&agent_status()).unwrap();
    }

    #[test]
    fn missing_and_unsupported_protocol_versions_are_rejected() {
        assert!(matches!(
            GrpcReplicaHandle::validate_protocol_version(0),
            Err(KubericError::RemoteControlProtocolUnsupported(_))
        ));
        assert!(matches!(
            GrpcReplicaHandle::validate_protocol_version(
                crate::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION + 1
            ),
            Err(KubericError::RemoteControlProtocolUnsupported(_))
        ));
    }

    #[test]
    fn strict_agent_status_rejects_malformed_ledger() {
        let mut status = agent_status();
        status.retained_terminal_actions[0].action.state = DurableActionState::InProgress;
        assert!(GrpcReplicaHandle::validate_agent_observations(&status).is_err());

        let mut status = agent_status();
        status.retained_terminal_actions[0].generation =
            AgentGeneration::from_string("other-generation");
        assert!(GrpcReplicaHandle::validate_agent_observations(&status).is_err());

        let mut status = agent_status();
        status.retained_terminal_actions[0].action.action_id = "current".to_string();
        assert!(GrpcReplicaHandle::validate_agent_observations(&status).is_err());

        let mut status = agent_status();
        status.retained_terminal_actions[0].control_version = AgentControlVersion::new(0);
        assert!(GrpcReplicaHandle::validate_agent_observations(&status).is_err());

        let mut status = agent_status();
        status.control_version = AgentControlVersion::new(7);
        status.current_action = None;
        status.retained_terminal_actions.clear();
        assert!(GrpcReplicaHandle::validate_agent_observations(&status).is_err());
    }

    #[test]
    fn malformed_runtime_status_is_rejected() {
        assert!(GrpcReplicaHandle::decode_write_status(None).is_err());
        assert!(crate::grpc::convert::try_access_status(999).is_err());
        for mode in [ReplicaConfigurationModeProto::ConfigurationNone as i32, 999] {
            assert!(
                GrpcReplicaHandle::decode_runtime_configuration(Some(
                    ReplicaConfigurationStatusProto {
                        mode,
                        members: Vec::new(),
                        write_quorum: 1,
                    }
                ))
                .is_err()
            );
        }
    }

    #[test]
    fn replica_agent_transport_classes_survive_client_mapping() {
        assert!(matches!(
            GrpcReplicaHandle::map_err(tonic::Status::resource_exhausted("replica agent is busy")),
            KubericError::AgentBusy
        ));
        assert!(matches!(
            GrpcReplicaHandle::map_err(tonic::Status::unimplemented("unsupported protocol")),
            KubericError::RemoteControlProtocolUnsupported(_)
        ));
    }
}
