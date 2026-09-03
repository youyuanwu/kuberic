use crate::replica_lifecycle::{
    ConfigurationDescriptor, ConfigurationProgressSource, MAX_LIFECYCLE_ERROR_BYTES,
    REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
};
use crate::types::{AgentGeneration, Epoch, Lsn, ReplicaId, ReplicaInstanceId, Role};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddReplicaMode {
    ScaleUp,
    Rebuild,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddReplicaIntent {
    pub operation_id: String,
    pub attempt_id: String,
    pub mode: AddReplicaMode,
    pub epoch: Epoch,
    pub primary_replica_id: ReplicaId,
    pub primary_instance_id: ReplicaInstanceId,
    pub primary_agent_generation: AgentGeneration,
    pub primary_control_address: String,
    pub target_replica_id: ReplicaId,
    pub target_instance_id: ReplicaInstanceId,
    pub target_agent_generation: AgentGeneration,
    pub target_control_address: String,
    pub target_replicator_address: String,
    pub retired_instance_id: Option<ReplicaInstanceId>,
    pub previous_configuration: ConfigurationDescriptor,
    pub catch_up_configuration: ConfigurationDescriptor,
    pub current_configuration: ConfigurationDescriptor,
    pub minimum_committed_replicas: u32,
    pub deadline_unix_seconds: i64,
    pub compensation_deadline_unix_seconds: i64,
    pub target_lifecycle_peer_protocol_version: u32,
}

impl AddReplicaIntent {
    pub fn validate(&self) -> Result<(), String> {
        if self.operation_id.is_empty()
            || self.attempt_id.is_empty()
            || self.primary_replica_id <= 0
            || self.target_replica_id <= 0
            || self.primary_replica_id == self.target_replica_id
            || self.primary_instance_id.as_str().is_empty()
            || self.target_instance_id.as_str().is_empty()
            || self.primary_control_address.is_empty()
            || self.target_control_address.is_empty()
            || self.target_replicator_address.is_empty()
            || self.minimum_committed_replicas == 0
            || self.deadline_unix_seconds <= 0
            || self.compensation_deadline_unix_seconds < self.deadline_unix_seconds
            || self.target_lifecycle_peer_protocol_version
                != REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION
        {
            return Err("add-replica intent has invalid identity, endpoint, or limit".to_string());
        }
        for descriptor in [
            &self.previous_configuration,
            &self.catch_up_configuration,
            &self.current_configuration,
        ] {
            let mut ids = std::collections::HashSet::new();
            let mut instances = std::collections::HashSet::new();
            if descriptor.write_quorum == 0
                || descriptor.write_quorum as usize > descriptor.members.len() + 1
            {
                return Err("add-replica configuration has invalid write quorum".to_string());
            }
            for member in &descriptor.members {
                if member.id <= 0
                    || !ids.insert(member.id)
                    || member.instance_id.as_str().is_empty()
                    || !instances.insert(member.instance_id.clone())
                    || member.replicator_address.is_empty()
                    || member.role != Role::ActiveSecondary
                {
                    return Err("add-replica configuration member is invalid".to_string());
                }
            }
        }
        let catch_up_target = self
            .catch_up_configuration
            .members
            .iter()
            .filter(|member| {
                member.id == self.target_replica_id && member.instance_id == self.target_instance_id
            })
            .collect::<Vec<_>>();
        let current_target = self
            .current_configuration
            .members
            .iter()
            .filter(|member| {
                member.id == self.target_replica_id && member.instance_id == self.target_instance_id
            })
            .collect::<Vec<_>>();
        if catch_up_target.len() != 1
            || current_target.len() != 1
            || !catch_up_target[0].must_catch_up
            || current_target[0].must_catch_up
            || catch_up_target[0].progress != ConfigurationProgressSource::BuildCopyLsn
            || current_target[0].progress != ConfigurationProgressSource::BuildCopyLsn
        {
            return Err("add-replica target configuration is inconsistent".to_string());
        }
        let total_members = self.current_configuration.members.len() + 1;
        let expected_quorum = total_members as u32 / 2 + 1;
        if self.current_configuration.write_quorum != expected_quorum
            || self.catch_up_configuration.write_quorum != expected_quorum
            || self.minimum_committed_replicas as usize > total_members
        {
            return Err("add-replica target quorum or minimum is inconsistent".to_string());
        }
        match self.mode {
            AddReplicaMode::ScaleUp => {
                if self.retired_instance_id.is_some()
                    || self.previous_configuration.members.iter().any(|member| {
                        member.id == self.target_replica_id
                            || member.instance_id == self.target_instance_id
                    })
                {
                    return Err("scale-up intent contains an existing target".to_string());
                }
            }
            AddReplicaMode::Rebuild => {
                let retired = self
                    .retired_instance_id
                    .as_ref()
                    .ok_or_else(|| "rebuild intent has no retired incarnation".to_string())?;
                if retired == &self.target_instance_id
                    || !self.previous_configuration.members.iter().any(|member| {
                        member.id == self.target_replica_id && &member.instance_id == retired
                    })
                {
                    return Err("rebuild intent does not identify the retired target".to_string());
                }
            }
        }
        Ok(())
    }

    pub fn semantic_build_key(&self) -> String {
        format!(
            "add-build:{}:{}@{}:{}:{}:{}:{}",
            self.operation_id,
            self.target_replica_id,
            self.target_instance_id,
            self.target_agent_generation,
            self.target_replicator_address,
            self.epoch.data_loss_number,
            self.epoch.configuration_number,
        ) + &format!(":{}", self.catch_up_configuration.signature())
    }

    pub fn configuration_fence(&self) -> String {
        format!(
            "previous={};catch-up={};current={}",
            self.previous_configuration.signature(),
            self.catch_up_configuration.signature(),
            self.current_configuration.signature()
        )
    }

    pub fn signature(&self) -> String {
        format!(
            "add-replica:{}:{}:{:?}:{}:{}:{}@{}:{}:{}:{}@{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
            self.operation_id,
            self.attempt_id,
            self.mode,
            self.epoch.data_loss_number,
            self.epoch.configuration_number,
            self.primary_replica_id,
            self.primary_instance_id,
            self.primary_agent_generation,
            self.primary_control_address,
            self.target_replica_id,
            self.target_instance_id,
            self.target_agent_generation,
            self.target_control_address,
            self.target_replicator_address,
            self.retired_instance_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| "none".to_string()),
            self.configuration_fence(),
            self.minimum_committed_replicas,
            self.deadline_unix_seconds,
            self.compensation_deadline_unix_seconds,
            self.target_lifecycle_peer_protocol_version
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddReplicaCoordinatorPhase {
    Validating,
    RetiringOldConnection,
    PreparingTarget,
    Building,
    ActivatingTarget,
    InstallingCatchUpConfiguration,
    WaitingForCatchUpQuorum,
    InstallingCurrentConfiguration,
    Attesting,
    Compensating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddReplicaTerminalResult {
    Committed,
    Compensated,
    CompensationIncomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddReplicaProgress {
    pub phase: AddReplicaCoordinatorPhase,
    pub commit_observed: bool,
    pub copy_lsn: Option<Lsn>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeBuildState {
    InProgress,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeBuildObservation {
    pub execution_id: String,
    pub build_key: String,
    pub target_replica_id: ReplicaId,
    pub target_instance_id: ReplicaInstanceId,
    pub target_agent_generation: AgentGeneration,
    pub state: RuntimeBuildState,
    pub copy_lsn: Option<Lsn>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeQuorumWaitObservation {
    pub execution_id: String,
    pub state: RuntimeBuildState,
    pub error: Option<String>,
}

pub fn normalize_add_error(error: &str) -> String {
    if error.len() <= MAX_LIFECYCLE_ERROR_BYTES {
        return error.to_string();
    }
    let mut boundary = MAX_LIFECYCLE_ERROR_BYTES;
    while !error.is_char_boundary(boundary) {
        boundary -= 1;
    }
    error[..boundary].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replica_lifecycle::ConfigurationMemberDescriptor;
    use crate::types::ReplicaStatus;

    fn generation(value: char) -> AgentGeneration {
        AgentGeneration::parse(value.to_string().repeat(32)).unwrap()
    }

    fn descriptor(build_progress: bool) -> ConfigurationDescriptor {
        ConfigurationDescriptor {
            members: vec![ConfigurationMemberDescriptor {
                id: 2,
                instance_id: ReplicaInstanceId::new("target"),
                role: Role::ActiveSecondary,
                status: ReplicaStatus::Up,
                replicator_address: "http://target:7001".to_string(),
                must_catch_up: build_progress,
                progress: if build_progress {
                    ConfigurationProgressSource::BuildCopyLsn
                } else {
                    ConfigurationProgressSource::Frozen {
                        current_progress: 4,
                        catch_up_capability: 4,
                    }
                },
            }],
            write_quorum: 2,
        }
    }

    fn intent() -> AddReplicaIntent {
        AddReplicaIntent {
            operation_id: "operation".to_string(),
            attempt_id: "attempt-1".to_string(),
            mode: AddReplicaMode::ScaleUp,
            epoch: Epoch::new(1, 2),
            primary_replica_id: 1,
            primary_instance_id: ReplicaInstanceId::new("primary"),
            primary_agent_generation: generation('a'),
            primary_control_address: "http://primary:7000".to_string(),
            target_replica_id: 2,
            target_instance_id: ReplicaInstanceId::new("target"),
            target_agent_generation: generation('b'),
            target_control_address: "http://target:7000".to_string(),
            target_replicator_address: "http://target:7001".to_string(),
            retired_instance_id: None,
            previous_configuration: ConfigurationDescriptor {
                members: Vec::new(),
                write_quorum: 1,
            },
            catch_up_configuration: descriptor(true),
            current_configuration: descriptor(false),
            minimum_committed_replicas: 1,
            deadline_unix_seconds: 100,
            compensation_deadline_unix_seconds: 110,
            target_lifecycle_peer_protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
        }
    }

    #[test]
    fn target_generation_changes_semantic_build_key() {
        let first = intent();
        let mut second = first.clone();
        second.target_agent_generation = generation('c');
        assert_ne!(first.semantic_build_key(), second.semantic_build_key());
    }
}
