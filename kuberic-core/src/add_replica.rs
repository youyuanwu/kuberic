use crate::types::{
    AgentGeneration, Epoch, Lsn, ReplicaConfigurationMemberStatus, ReplicaConfigurationMode,
    ReplicaConfigurationStatus, ReplicaId, ReplicaInfo, ReplicaInstanceId, ReplicaSetConfig,
    ReplicaStatus, Role,
};

pub const REPLICA_ADD_BUILD_PEER_PROTOCOL_VERSION: u32 = 1;
pub const PEER_TERMINAL_RETENTION: usize = 16;
pub const MAX_ADD_ERROR_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddReplicaMode {
    ScaleUp,
    Rebuild,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigurationProgressSource {
    Frozen {
        current_progress: Lsn,
        catch_up_capability: Lsn,
    },
    BuildCopyLsn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationMemberDescriptor {
    pub id: ReplicaId,
    pub instance_id: ReplicaInstanceId,
    pub role: Role,
    pub status: ReplicaStatus,
    pub replicator_address: String,
    pub must_catch_up: bool,
    pub progress: ConfigurationProgressSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigurationDescriptor {
    pub members: Vec<ConfigurationMemberDescriptor>,
    pub write_quorum: u32,
}

impl ConfigurationDescriptor {
    pub fn signature(&self) -> String {
        let mut members = self
            .members
            .iter()
            .map(|member| {
                let progress = match member.progress {
                    ConfigurationProgressSource::Frozen {
                        current_progress,
                        catch_up_capability,
                    } => format!("frozen:{current_progress}:{catch_up_capability}"),
                    ConfigurationProgressSource::BuildCopyLsn => "build-copy-lsn".to_string(),
                };
                format!(
                    "{}@{}:{:?}:{:?}:{}:{}:{}",
                    member.id,
                    member.instance_id,
                    member.role,
                    member.status,
                    member.replicator_address,
                    member.must_catch_up,
                    progress
                )
            })
            .collect::<Vec<_>>();
        members.sort();
        format!("q{}[{}]", self.write_quorum, members.join(","))
    }

    pub fn materialize(&self, copy_lsn: Option<Lsn>) -> Result<ReplicaSetConfig, String> {
        let mut members = Vec::with_capacity(self.members.len());
        for member in &self.members {
            let (current_progress, catch_up_capability) = match member.progress {
                ConfigurationProgressSource::Frozen {
                    current_progress,
                    catch_up_capability,
                } => (current_progress, catch_up_capability),
                ConfigurationProgressSource::BuildCopyLsn => {
                    let copy_lsn = copy_lsn.ok_or_else(|| {
                        format!(
                            "configuration member {} requires a completed copy LSN",
                            member.id
                        )
                    })?;
                    (copy_lsn, copy_lsn)
                }
            };
            members.push(ReplicaInfo {
                id: member.id,
                instance_id: member.instance_id.clone(),
                role: member.role,
                status: member.status,
                replicator_address: member.replicator_address.clone(),
                current_progress,
                catch_up_capability,
                must_catch_up: member.must_catch_up,
            });
        }
        members.sort_by_key(|member| member.id);
        Ok(ReplicaSetConfig {
            members,
            write_quorum: self.write_quorum,
        })
    }

    pub fn status(&self, mode: ReplicaConfigurationMode) -> ReplicaConfigurationStatus {
        let mut members = self
            .members
            .iter()
            .map(|member| ReplicaConfigurationMemberStatus {
                id: member.id,
                instance_id: member.instance_id.clone(),
                role: member.role,
            })
            .collect::<Vec<_>>();
        members.sort_by_key(|member| member.id);
        ReplicaConfigurationStatus {
            mode,
            members,
            write_quorum: self.write_quorum,
        }
    }
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
            "add-replica:{}:{}:{:?}:{}:{}:{}@{}:{}:{}:{}@{}:{}:{}:{}:{}:{}:{}:{}:{}",
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
            self.compensation_deadline_unix_seconds
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStage {
    Prepare,
    Activate,
    Cleanup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStageState {
    Accepted,
    InProgress,
    Completed,
    Failed,
    Stale,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStageRequest {
    pub protocol_version: u32,
    pub operation_id: String,
    pub attempt_id: String,
    pub message_id: String,
    pub input_signature: String,
    pub stage: PeerStage,
    pub sender_replica_id: ReplicaId,
    pub sender_instance_id: ReplicaInstanceId,
    pub sender_agent_generation: AgentGeneration,
    pub sender_control_address: String,
    pub parent_action_id: String,
    pub parent_action_signature: String,
    pub target_replica_id: ReplicaId,
    pub target_instance_id: ReplicaInstanceId,
    pub expected_target_agent_generation: AgentGeneration,
    pub expected_target_peer_control_version: u64,
    pub epoch: Epoch,
    pub configuration_fence: String,
    pub build_key: Option<String>,
    pub copy_lsn: Option<Lsn>,
}

impl PeerStageRequest {
    pub fn signature(&self) -> String {
        format!(
            "peer:{}:{}:{}:{:?}:{}@{}:{}:{}:{}:{}:{}@{}:{}:{}:{}:{}:{}:{}:{}",
            self.operation_id,
            self.attempt_id,
            self.message_id,
            self.stage,
            self.sender_replica_id,
            self.sender_instance_id,
            self.sender_agent_generation,
            self.sender_control_address,
            self.parent_action_id,
            self.parent_action_signature,
            self.target_replica_id,
            self.target_instance_id,
            self.expected_target_agent_generation,
            self.expected_target_peer_control_version,
            self.epoch.data_loss_number,
            self.epoch.configuration_number,
            self.configuration_fence,
            self.build_key.as_deref().unwrap_or("none"),
            self.copy_lsn
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string())
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStageObservation {
    pub protocol_version: u32,
    pub message_id: String,
    pub input_signature: String,
    pub stage: PeerStage,
    pub state: PeerStageState,
    pub target_agent_generation: AgentGeneration,
    pub target_peer_control_version: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddBuildStatus {
    pub protocol_version: u32,
    pub target_replica_id: ReplicaId,
    pub target_instance_id: ReplicaInstanceId,
    pub agent_generation: AgentGeneration,
    pub peer_control_version: u64,
    pub role: Role,
    pub epoch: Epoch,
    pub healthy: bool,
    pub current_progress: Lsn,
    pub current_action: Option<PeerStageObservation>,
    pub retained_terminal_actions: Vec<PeerStageObservation>,
}

pub fn normalize_add_error(error: &str) -> String {
    if error.len() <= MAX_ADD_ERROR_BYTES {
        return error.to_string();
    }
    let mut boundary = MAX_ADD_ERROR_BYTES;
    while !error.is_char_boundary(boundary) {
        boundary -= 1;
    }
    error[..boundary].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
        }
    }

    #[test]
    fn target_generation_changes_semantic_build_key() {
        let first = intent();
        let mut second = first.clone();
        second.target_agent_generation = generation('c');
        assert_ne!(first.semantic_build_key(), second.semantic_build_key());
    }

    #[test]
    fn descriptor_materializes_copy_progress() {
        let config = descriptor(true).materialize(Some(42)).unwrap();
        assert_eq!(config.members[0].current_progress, 42);
        assert_eq!(config.members[0].catch_up_capability, 42);
    }

    #[test]
    fn descriptor_signature_is_member_order_independent() {
        let mut first = descriptor(false);
        first.members.push(ConfigurationMemberDescriptor {
            id: 3,
            instance_id: ReplicaInstanceId::new("three"),
            role: Role::ActiveSecondary,
            status: ReplicaStatus::Up,
            replicator_address: "http://three".to_string(),
            must_catch_up: false,
            progress: ConfigurationProgressSource::Frozen {
                current_progress: 3,
                catch_up_capability: 3,
            },
        });
        let mut second = first.clone();
        second.members.reverse();
        assert_eq!(first.signature(), second.signature());
    }
}
