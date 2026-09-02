use std::collections::HashSet;

use tonic::transport::Endpoint;

use crate::remove_replica::{REMOVE_REPLICA_RETIREMENT_TIMEOUT_SECONDS, RemoveReplicaMode};
use crate::types::{
    AgentGeneration, Epoch, Lsn, ReplicaConfigurationMemberStatus, ReplicaConfigurationMode,
    ReplicaConfigurationStatus, ReplicaId, ReplicaInfo, ReplicaInstanceId, ReplicaSetConfig,
    ReplicaStatus, Role,
};

pub const REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION: u32 = 2;
pub const PEER_STAGE_SEMANTIC_VERSION: u32 = 1;
pub const RETIRE_STAGE_SEMANTIC_VERSION: u32 = 1;
pub const PEER_TERMINAL_RETENTION: usize = 16;
pub const MAX_LIFECYCLE_ERROR_BYTES: usize = 1024;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerOperationKind {
    AddBuild,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStage {
    Prepare,
    Activate,
    Cleanup,
    Retire,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerStageState {
    Accepted,
    InProgress,
    Completed,
    Failed,
    Stale,
    Rejected,
    Conflict,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStageRequest {
    pub protocol_version: u32,
    pub operation_kind: PeerOperationKind,
    pub stage_semantic_version: u32,
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
    pub removal_mode: Option<RemoveReplicaMode>,
    pub commit_observed_unix_seconds: Option<i64>,
    pub retirement_expiry_unix_seconds: Option<i64>,
    pub reduced_current_projection: Option<ReplicaConfigurationStatus>,
}

impl PeerStageRequest {
    pub fn signature(&self) -> String {
        let add_build_signature = format!(
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
        );
        if self.operation_kind == PeerOperationKind::AddBuild
            && self.removal_mode.is_none()
            && self.commit_observed_unix_seconds.is_none()
            && self.retirement_expiry_unix_seconds.is_none()
            && self.reduced_current_projection.is_none()
        {
            return add_build_signature;
        }

        format!(
            "peer-lifecycle-v{}:{:?}:stage-v{}:{}:remove-mode={}:commit={}:retirement-expiry={}:projection={}",
            self.protocol_version,
            self.operation_kind,
            self.stage_semantic_version,
            add_build_signature,
            self.removal_mode
                .map(|mode| format!("{mode:?}"))
                .unwrap_or_else(|| "none".to_string()),
            self.commit_observed_unix_seconds
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string()),
            self.retirement_expiry_unix_seconds
                .map(|value| value.to_string())
                .unwrap_or_else(|| "none".to_string()),
            self.reduced_current_projection
                .as_ref()
                .map(reduced_current_projection_signature)
                .unwrap_or_else(|| "none".to_string()),
        )
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.operation_id.is_empty()
            || self.attempt_id.is_empty()
            || self.message_id.is_empty()
            || self.input_signature.is_empty()
            || self.parent_action_id.is_empty()
            || self.parent_action_signature.is_empty()
            || self.sender_control_address.is_empty()
            || self.configuration_fence.is_empty()
        {
            return Err("peer stage request has missing required text".to_string());
        }
        if self.sender_replica_id <= 0
            || self.target_replica_id <= 0
            || self.sender_replica_id == self.target_replica_id
            || self.sender_instance_id.as_str().is_empty()
            || self.target_instance_id.as_str().is_empty()
        {
            return Err("peer stage request identity is invalid".to_string());
        }
        Endpoint::from_shared(self.sender_control_address.clone())
            .map_err(|_| "peer stage sender control endpoint is invalid".to_string())?;

        match self.operation_kind {
            PeerOperationKind::AddBuild => self.validate_add_build_fields()?,
            PeerOperationKind::Remove => self.validate_retire_fields()?,
        }
        if self.input_signature != self.signature() {
            return Err("peer stage signature does not match its payload".to_string());
        }
        Ok(())
    }

    fn validate_add_build_fields(&self) -> Result<(), String> {
        if self.protocol_version != REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION
            || self.stage_semantic_version != PEER_STAGE_SEMANTIC_VERSION
            || self.stage == PeerStage::Retire
        {
            return Err(
                "lifecycle peer v2 accepts only valid add/build stage semantics".to_string(),
            );
        }
        if self.removal_mode.is_some()
            || self.commit_observed_unix_seconds.is_some()
            || self.retirement_expiry_unix_seconds.is_some()
            || self.reduced_current_projection.is_some()
        {
            return Err("add/build peer stage carries removal-only fields".to_string());
        }
        if self.stage == PeerStage::Activate
            && (self.build_key.as_deref().is_none_or(str::is_empty) || self.copy_lsn.is_none())
        {
            return Err("activate peer stage requires build proof".to_string());
        }
        if self.stage != PeerStage::Activate
            && (self.build_key.is_some() || self.copy_lsn.is_some())
        {
            return Err("non-activate peer stage carries build proof".to_string());
        }
        Ok(())
    }

    fn validate_retire_fields(&self) -> Result<(), String> {
        if self.protocol_version != REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION
            || self.stage != PeerStage::Retire
            || self.stage_semantic_version != RETIRE_STAGE_SEMANTIC_VERSION
        {
            return Err(
                "remove peer operation requires Retire stage v1 over lifecycle peer v2".to_string(),
            );
        }
        if self.build_key.is_some() || self.copy_lsn.is_some() {
            return Err("Retire peer stage carries add/build proof".to_string());
        }
        self.removal_mode
            .ok_or_else(|| "Retire peer stage has no removal mode".to_string())?;
        let commit_observed = self
            .commit_observed_unix_seconds
            .ok_or_else(|| "Retire peer stage has no commit observation time".to_string())?;
        let retirement_expiry = self
            .retirement_expiry_unix_seconds
            .ok_or_else(|| "Retire peer stage has no retirement expiry".to_string())?;
        if commit_observed <= 0
            || retirement_expiry < commit_observed
            || retirement_expiry
                > commit_observed.saturating_add(REMOVE_REPLICA_RETIREMENT_TIMEOUT_SECONDS)
        {
            return Err("Retire peer stage has an invalid commit/deadline fence".to_string());
        }
        let projection = self
            .reduced_current_projection
            .as_ref()
            .ok_or_else(|| "Retire peer stage has no reduced Current projection".to_string())?;
        validate_reduced_current_projection(projection)?;
        if projection.members.iter().any(|member| {
            member.id == self.target_replica_id && member.instance_id == self.target_instance_id
        }) {
            return Err("Retire target remains in the reduced Current projection".to_string());
        }
        if !self.configuration_fence.starts_with("previous=")
            || !self.configuration_fence.contains(";reduced-catch-up=")
            || !self.configuration_fence.contains(";reduced-current=")
            || !self
                .parent_action_signature
                .contains(&format!(":{}:", self.configuration_fence))
        {
            return Err(
                "Retire projection/configuration fence does not match the parent signature"
                    .to_string(),
            );
        }
        validate_projection_against_configuration_fence(projection, &self.configuration_fence)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerStageObservation {
    pub protocol_version: u32,
    pub operation_kind: PeerOperationKind,
    pub stage_semantic_version: u32,
    pub message_id: String,
    pub input_signature: String,
    pub stage: PeerStage,
    pub state: PeerStageState,
    pub target_agent_generation: AgentGeneration,
    pub target_peer_control_version: u64,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerLifecycleStatus {
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

pub fn validate_reduced_current_projection(
    projection: &ReplicaConfigurationStatus,
) -> Result<(), String> {
    if projection.mode != ReplicaConfigurationMode::Current {
        return Err("reduced current projection is not Current".to_string());
    }
    if projection.write_quorum == 0
        || projection.write_quorum as usize > projection.members.len() + 1
    {
        return Err("reduced current projection write quorum is invalid".to_string());
    }
    let mut ids = HashSet::new();
    let mut instances = HashSet::new();
    let mut previous_id = None;
    for member in &projection.members {
        if member.id <= 0
            || member.instance_id.as_str().is_empty()
            || member.role == Role::Unknown
            || !ids.insert(member.id)
            || !instances.insert(member.instance_id.clone())
            || previous_id.is_some_and(|id| member.id <= id)
        {
            return Err(
                "reduced current projection members are invalid or not strictly sorted".to_string(),
            );
        }
        previous_id = Some(member.id);
    }
    Ok(())
}

pub fn reduced_current_projection_signature(projection: &ReplicaConfigurationStatus) -> String {
    let members = projection
        .members
        .iter()
        .map(|member| format!("{}@{}:{:?}", member.id, member.instance_id, member.role))
        .collect::<Vec<_>>()
        .join(",");
    format!(
        "{:?}:q{}[{}]",
        projection.mode, projection.write_quorum, members
    )
}

fn validate_projection_against_configuration_fence(
    projection: &ReplicaConfigurationStatus,
    configuration_fence: &str,
) -> Result<(), String> {
    let reduced = configuration_fence
        .split(";reduced-current=")
        .nth(1)
        .ok_or_else(|| {
            "Retire configuration fence has no reduced Current descriptor".to_string()
        })?;
    let expected_prefix = format!("q{}[", projection.write_quorum);
    if !reduced.starts_with(&expected_prefix) {
        return Err(
            "Retire projection write quorum does not match the configuration fence".to_string(),
        );
    }
    for member in &projection.members {
        let identity_role = format!("{}@{}:{:?}:", member.id, member.instance_id, member.role);
        if !reduced.contains(&identity_role) {
            return Err(
                "Retire projection membership does not match the configuration fence".to_string(),
            );
        }
    }
    let descriptor_member_count = reduced
        .strip_prefix(&expected_prefix)
        .and_then(|value| value.strip_suffix(']'))
        .map_or(0, |members| {
            if members.is_empty() {
                0
            } else {
                members.split(',').count()
            }
        });
    if descriptor_member_count != projection.members.len() {
        return Err(
            "Retire projection member count does not match the configuration fence".to_string(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn retire_request() -> PeerStageRequest {
        let previous = "q2[2@target:ActiveSecondary:Up:http://replica-2:7001:false:frozen:20:20,3@retained:ActiveSecondary:Up:http://replica-3:7001:false:frozen:30:30]";
        let reduced = "q2[3@retained:ActiveSecondary:Up:http://replica-3:7001:false:frozen:30:30]";
        let configuration_fence =
            format!("previous={previous};reduced-catch-up={reduced};reduced-current={reduced}");
        let mut request = PeerStageRequest {
            protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
            operation_kind: PeerOperationKind::Remove,
            stage_semantic_version: RETIRE_STAGE_SEMANTIC_VERSION,
            operation_id: "remove-operation".to_string(),
            attempt_id: "attempt-1".to_string(),
            message_id: "retire".to_string(),
            input_signature: String::new(),
            stage: PeerStage::Retire,
            sender_replica_id: 1,
            sender_instance_id: ReplicaInstanceId::new("primary"),
            sender_agent_generation: AgentGeneration::parse("0123456789abcdef0123456789abcdef")
                .unwrap(),
            sender_control_address: "http://primary".to_string(),
            parent_action_id: "remove-parent".to_string(),
            parent_action_signature: format!("remove-parent:{configuration_fence}:2"),
            target_replica_id: 2,
            target_instance_id: ReplicaInstanceId::new("target"),
            expected_target_agent_generation: AgentGeneration::parse(
                "11111111111111111111111111111111",
            )
            .unwrap(),
            expected_target_peer_control_version: 7,
            epoch: Epoch::new(1, 2),
            configuration_fence,
            build_key: None,
            copy_lsn: None,
            removal_mode: Some(RemoveReplicaMode::ScaleDown),
            commit_observed_unix_seconds: Some(100),
            retirement_expiry_unix_seconds: Some(160),
            reduced_current_projection: Some(ReplicaConfigurationStatus {
                mode: ReplicaConfigurationMode::Current,
                members: vec![ReplicaConfigurationMemberStatus {
                    id: 3,
                    instance_id: ReplicaInstanceId::new("retained"),
                    role: Role::ActiveSecondary,
                }],
                write_quorum: 2,
            }),
        };
        request.input_signature = request.signature();
        request
    }

    #[test]
    fn descriptor_materialization_is_unchanged_after_extraction() {
        let materialized = descriptor(true).materialize(Some(42)).unwrap();
        assert_eq!(materialized.write_quorum, 2);
        assert_eq!(materialized.members.len(), 1);
        let member = &materialized.members[0];
        assert_eq!(member.id, 2);
        assert_eq!(member.instance_id, ReplicaInstanceId::new("target"));
        assert_eq!(member.role, Role::ActiveSecondary);
        assert_eq!(member.status, ReplicaStatus::Up);
        assert_eq!(member.replicator_address, "http://target:7001");
        assert_eq!(member.current_progress, 42);
        assert_eq!(member.catch_up_capability, 42);
        assert!(member.must_catch_up);
    }

    #[test]
    fn descriptor_signature_is_unchanged_after_extraction() {
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
        let expected = "q2[2@target:ActiveSecondary:Up:http://target:7001:false:frozen:4:4,3@three:ActiveSecondary:Up:http://three:false:frozen:3:3]";
        assert_eq!(first.signature(), expected);

        first.members.reverse();
        assert_eq!(first.signature(), expected);
    }

    #[test]
    fn add_build_lifecycle_stage_signature_is_unchanged_after_extraction() {
        let mut request = PeerStageRequest {
            protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
            operation_kind: PeerOperationKind::AddBuild,
            stage_semantic_version: PEER_STAGE_SEMANTIC_VERSION,
            operation_id: "operation".to_string(),
            attempt_id: "attempt-1".to_string(),
            message_id: "prepare".to_string(),
            input_signature: String::new(),
            stage: PeerStage::Prepare,
            sender_replica_id: 1,
            sender_instance_id: ReplicaInstanceId::new("primary"),
            sender_agent_generation: AgentGeneration::parse("0123456789abcdef0123456789abcdef")
                .unwrap(),
            sender_control_address: "http://primary".to_string(),
            parent_action_id: "parent".to_string(),
            parent_action_signature: "parent-signature".to_string(),
            target_replica_id: 2,
            target_instance_id: ReplicaInstanceId::new("target"),
            expected_target_agent_generation: AgentGeneration::parse(
                "11111111111111111111111111111111",
            )
            .unwrap(),
            expected_target_peer_control_version: 7,
            epoch: Epoch::new(1, 2),
            configuration_fence: "fence".to_string(),
            build_key: None,
            copy_lsn: None,
            removal_mode: None,
            commit_observed_unix_seconds: None,
            retirement_expiry_unix_seconds: None,
            reduced_current_projection: None,
        };
        request.input_signature = request.signature();
        request.validate().unwrap();

        assert_eq!(
            request.input_signature,
            "peer:operation:attempt-1:prepare:Prepare:1@primary:0123456789abcdef0123456789abcdef:http://primary:parent:parent-signature:2@target:11111111111111111111111111111111:7:1:2:fence:none:none"
        );
    }

    #[test]
    fn retire_v1_requires_exact_removal_only_fields_and_signature() {
        let request = retire_request();
        request.validate().unwrap();
        assert!(
            request
                .input_signature
                .contains("peer-lifecycle-v2:Remove:stage-v1")
        );
        assert!(request.input_signature.contains("remove-mode=ScaleDown"));
        assert!(
            request
                .input_signature
                .contains("projection=Current:q2[3@retained:ActiveSecondary]")
        );

        let mut missing_projection = request.clone();
        missing_projection.reduced_current_projection = None;
        missing_projection.input_signature = missing_projection.signature();
        assert!(missing_projection.validate().is_err());

        let mut catch_up = request.clone();
        catch_up.reduced_current_projection.as_mut().unwrap().mode =
            ReplicaConfigurationMode::CatchUp;
        catch_up.input_signature = catch_up.signature();
        assert!(catch_up.validate().is_err());

        let mut stale_signature = request.clone();
        stale_signature
            .reduced_current_projection
            .as_mut()
            .unwrap()
            .members[0]
            .role = Role::IdleSecondary;
        assert!(stale_signature.validate().is_err());

        let mut mismatched_fence = request;
        mismatched_fence.configuration_fence.push_str("-changed");
        mismatched_fence.input_signature = mismatched_fence.signature();
        assert!(mismatched_fence.validate().is_err());
    }

    #[test]
    fn add_build_and_retire_stage_fields_cannot_mix() {
        let mut add = PeerStageRequest {
            protocol_version: REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
            operation_kind: PeerOperationKind::AddBuild,
            stage_semantic_version: PEER_STAGE_SEMANTIC_VERSION,
            operation_id: "operation".to_string(),
            attempt_id: "attempt".to_string(),
            message_id: "prepare".to_string(),
            input_signature: String::new(),
            stage: PeerStage::Prepare,
            sender_replica_id: 1,
            sender_instance_id: ReplicaInstanceId::new("primary"),
            sender_agent_generation: AgentGeneration::parse("0123456789abcdef0123456789abcdef")
                .unwrap(),
            sender_control_address: "http://primary".to_string(),
            parent_action_id: "parent".to_string(),
            parent_action_signature: "parent-signature".to_string(),
            target_replica_id: 2,
            target_instance_id: ReplicaInstanceId::new("target"),
            expected_target_agent_generation: AgentGeneration::parse(
                "11111111111111111111111111111111",
            )
            .unwrap(),
            expected_target_peer_control_version: 0,
            epoch: Epoch::new(1, 2),
            configuration_fence: "configuration".to_string(),
            build_key: None,
            copy_lsn: None,
            removal_mode: Some(RemoveReplicaMode::Force),
            commit_observed_unix_seconds: None,
            retirement_expiry_unix_seconds: None,
            reduced_current_projection: None,
        };
        add.input_signature = add.signature();
        assert!(add.validate().is_err());

        let mut retire = retire_request();
        retire.build_key = Some("build".to_string());
        retire.input_signature = retire.signature();
        assert!(retire.validate().is_err());

        let mut wrong_stage = retire_request();
        wrong_stage.stage = PeerStage::Cleanup;
        wrong_stage.input_signature = wrong_stage.signature();
        assert!(wrong_stage.validate().is_err());
    }

    #[test]
    fn retire_target_absence_is_exact_incarnation_aware() {
        let mut request = retire_request();
        let replacement =
            "q2[2@replacement:ActiveSecondary:Up:http://replica-2:7001:false:frozen:20:20]";
        request.reduced_current_projection = Some(ReplicaConfigurationStatus {
            mode: ReplicaConfigurationMode::Current,
            members: vec![ReplicaConfigurationMemberStatus {
                id: request.target_replica_id,
                instance_id: ReplicaInstanceId::new("replacement"),
                role: Role::ActiveSecondary,
            }],
            write_quorum: 2,
        });
        request.configuration_fence = format!(
            "previous={};reduced-catch-up={replacement};reduced-current={replacement}",
            request
                .configuration_fence
                .split(";reduced-catch-up=")
                .next()
                .unwrap()
                .strip_prefix("previous=")
                .unwrap()
        );
        request.parent_action_signature =
            format!("remove-parent:{}:2", request.configuration_fence);
        request.input_signature = request.signature();
        request.validate().unwrap();

        request.reduced_current_projection.as_mut().unwrap().members[0].instance_id =
            request.target_instance_id.clone();
        request.input_signature = request.signature();
        assert!(request.validate().is_err());
    }
}
