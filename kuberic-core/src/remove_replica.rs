use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use tonic::transport::Endpoint;

use crate::replica_lifecycle::{
    ConfigurationDescriptor, ConfigurationProgressSource, MAX_LIFECYCLE_ERROR_BYTES,
    REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION,
};
use crate::types::{
    AgentControlVersion, AgentGeneration, Epoch, ReplicaConfigurationMode,
    ReplicaConfigurationStatus, ReplicaId, ReplicaInstanceId, ReplicaStatus, Role,
};

pub const REMOVE_REPLICA_INTENT_PROTOCOL_VERSION: u32 = 1;
pub const MAX_REMOVE_REPLICA_PRE_COMMIT_ATTEMPTS: u32 = 3;
pub const REMOVE_REPLICA_OVERALL_TIMEOUT_SECONDS: i64 = 600;
pub const REMOVE_REPLICA_COMPENSATION_GRACE_SECONDS: i64 = 30;
pub const REMOVE_REPLICA_CALL_TIMEOUT_SECONDS: i64 = 10;
pub const REMOVE_REPLICA_RETIREMENT_TIMEOUT_SECONDS: i64 = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveReplicaMode {
    ScaleDown,
    Force,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveReplicaIntent {
    pub protocol_version: u32,
    pub operation_id: String,
    pub action_id: String,
    pub attempt_number: u32,
    pub attempt_id: String,
    pub input_signature: String,
    pub mode: RemoveReplicaMode,
    pub epoch: Epoch,
    pub primary_replica_id: ReplicaId,
    pub primary_instance_id: ReplicaInstanceId,
    pub primary_agent_generation: AgentGeneration,
    pub primary_agent_control_version: AgentControlVersion,
    pub primary_control_address: String,
    pub primary_replicator_address: String,
    pub target_replica_id: ReplicaId,
    pub target_instance_id: ReplicaInstanceId,
    pub expected_target_pod_uid: String,
    pub target_pod_name: String,
    pub expected_target_agent_generation: Option<AgentGeneration>,
    pub target_control_address: Option<String>,
    pub target_replicator_address: Option<String>,
    pub target_lifecycle_peer_protocol_version: Option<u32>,
    pub previous_configuration: ConfigurationDescriptor,
    pub reduced_catch_up_configuration: ConfigurationDescriptor,
    pub reduced_current_configuration: ConfigurationDescriptor,
    pub required_write_quorum: u32,
    pub minimum_committed_replicas: u32,
    pub maximum_pre_commit_attempts: u32,
    pub overall_deadline_unix_seconds: i64,
    pub compensation_grace_seconds: i64,
    pub compensation_deadline_cap_unix_seconds: i64,
    pub call_timeout_seconds: i64,
    pub target_retirement_timeout_seconds: i64,
}

impl RemoveReplicaIntent {
    pub fn validate(&self) -> Result<(), String> {
        if self.protocol_version != REMOVE_REPLICA_INTENT_PROTOCOL_VERSION {
            return Err(format!(
                "unsupported remove-replica intent protocol version {}",
                self.protocol_version
            ));
        }
        if self.operation_id.is_empty()
            || self.action_id.is_empty()
            || self.attempt_id.is_empty()
            || self.input_signature.is_empty()
        {
            return Err("remove-replica intent identity is missing".to_string());
        }
        if self.attempt_number == 0
            || self.maximum_pre_commit_attempts != MAX_REMOVE_REPLICA_PRE_COMMIT_ATTEMPTS
            || self.attempt_number > self.maximum_pre_commit_attempts
        {
            return Err("remove-replica attempt bounds are invalid".to_string());
        }
        if self.primary_replica_id <= 0
            || self.target_replica_id <= 0
            || self.primary_replica_id == self.target_replica_id
            || self.primary_instance_id.as_str().is_empty()
            || self.target_instance_id.as_str().is_empty()
            || self.expected_target_pod_uid.is_empty()
            || self.target_pod_name.is_empty()
        {
            return Err("remove-replica primary or target identity is invalid".to_string());
        }
        if self.target_instance_id.as_str() != self.expected_target_pod_uid {
            return Err(
                "remove-replica target runtime incarnation and Kubernetes pod UID differ"
                    .to_string(),
            );
        }
        validate_endpoint("primary control", &self.primary_control_address)?;
        validate_endpoint("primary replicator", &self.primary_replicator_address)?;
        validate_target_observability(self)?;
        validate_descriptor("previous", &self.previous_configuration)?;
        validate_descriptor("reduced catch-up", &self.reduced_catch_up_configuration)?;
        validate_descriptor("reduced current", &self.reduced_current_configuration)?;
        validate_configuration_relationship(self)?;
        validate_limits(self)?;
        if self.input_signature != self.signature() {
            return Err("remove-replica intent signature does not match its payload".to_string());
        }
        Ok(())
    }

    pub fn configuration_fence(&self) -> String {
        format!(
            "previous={};reduced-catch-up={};reduced-current={}",
            self.previous_configuration.signature(),
            self.reduced_catch_up_configuration.signature(),
            self.reduced_current_configuration.signature()
        )
    }

    pub fn previous_status(&self) -> ReplicaConfigurationStatus {
        self.previous_configuration
            .status(ReplicaConfigurationMode::Current)
    }

    pub fn reduced_catch_up_status(&self) -> ReplicaConfigurationStatus {
        self.reduced_catch_up_configuration
            .status(ReplicaConfigurationMode::CatchUp)
    }

    pub fn reduced_current_status(&self) -> ReplicaConfigurationStatus {
        self.reduced_current_configuration
            .status(ReplicaConfigurationMode::Current)
    }

    pub fn retirement_expiry(&self, commit_observed_unix_seconds: i64) -> Result<i64, String> {
        if commit_observed_unix_seconds <= 0
            || commit_observed_unix_seconds > self.overall_deadline_unix_seconds
        {
            return Err("remove-replica commit observation time is invalid".to_string());
        }
        Ok(commit_observed_unix_seconds
            .checked_add(self.target_retirement_timeout_seconds)
            .ok_or_else(|| "remove-replica retirement expiry overflows".to_string())?
            .min(self.overall_deadline_unix_seconds))
    }

    pub fn compensation_expiry(&self, failure_observed_unix_seconds: i64) -> Result<i64, String> {
        if failure_observed_unix_seconds <= 0
            || failure_observed_unix_seconds > self.compensation_deadline_cap_unix_seconds
        {
            return Err("remove-replica failure observation time is invalid".to_string());
        }
        Ok(failure_observed_unix_seconds
            .checked_add(self.compensation_grace_seconds)
            .ok_or_else(|| "remove-replica compensation expiry overflows".to_string())?
            .min(self.compensation_deadline_cap_unix_seconds))
    }

    pub fn validate_progress(&self, progress: &RemoveReplicaProgress) -> Result<(), String> {
        progress.validate()?;
        if progress.attempt_id != self.attempt_id {
            return Err("remove-replica progress attempt does not match its intent".to_string());
        }
        if let Some(commit_time) = progress.commit_observed_unix_seconds
            && progress.retirement_expiry_unix_seconds != Some(self.retirement_expiry(commit_time)?)
        {
            return Err(
                "remove-replica progress retirement expiry is not deterministic".to_string(),
            );
        }
        if progress
            .compensation_expiry_unix_seconds
            .is_some_and(|expiry| expiry > self.compensation_deadline_cap_unix_seconds)
        {
            return Err("remove-replica progress exceeds the compensation cap".to_string());
        }
        Ok(())
    }

    pub fn validate_terminal_progress(
        &self,
        progress: &RemoveReplicaProgress,
        result: RemoveReplicaTerminalResult,
    ) -> Result<(), String> {
        self.validate_progress(progress)?;
        progress.validate_terminal(result)
    }

    pub fn signature(&self) -> String {
        [
            format!("remove-replica-v{}", self.protocol_version),
            self.operation_id.clone(),
            self.action_id.clone(),
            self.attempt_number.to_string(),
            self.attempt_id.clone(),
            format!("{:?}", self.mode),
            self.epoch.data_loss_number.to_string(),
            self.epoch.configuration_number.to_string(),
            format!("{}@{}", self.primary_replica_id, self.primary_instance_id),
            self.primary_agent_generation.to_string(),
            self.primary_agent_control_version.value().to_string(),
            self.primary_control_address.clone(),
            self.primary_replicator_address.clone(),
            format!("{}@{}", self.target_replica_id, self.target_instance_id),
            self.expected_target_pod_uid.clone(),
            self.target_pod_name.clone(),
            optional_generation(&self.expected_target_agent_generation).to_string(),
            optional_text(self.target_control_address.as_deref()).to_string(),
            optional_text(self.target_replicator_address.as_deref()).to_string(),
            self.target_lifecycle_peer_protocol_version
                .map_or_else(|| "none".to_string(), |value| value.to_string()),
            self.configuration_fence(),
            self.required_write_quorum.to_string(),
            self.minimum_committed_replicas.to_string(),
            self.maximum_pre_commit_attempts.to_string(),
            self.overall_deadline_unix_seconds.to_string(),
            self.compensation_grace_seconds.to_string(),
            self.compensation_deadline_cap_unix_seconds.to_string(),
            self.call_timeout_seconds.to_string(),
            self.target_retirement_timeout_seconds.to_string(),
        ]
        .join(":")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveReplicaCoordinatorPhase {
    Validating,
    InstallingCatchUpConfiguration,
    WaitingForCatchUpQuorum,
    InstallingCurrentConfiguration,
    RemovingConnection,
    RetiringTarget,
    Attesting,
    Compensating,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetRetirementObservation {
    NotAttempted,
    InProgress,
    Completed,
    Unavailable,
    Stale,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveReplicaTerminalResult {
    CommittedClean,
    CommittedDegraded,
    Compensated,
    CompensationIncomplete,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoveReplicaProgress {
    pub phase: RemoveReplicaCoordinatorPhase,
    pub attempt_id: String,
    pub commit_observed: bool,
    pub commit_observed_unix_seconds: Option<i64>,
    pub connection_absent: bool,
    pub target_retirement: TargetRetirementObservation,
    pub retirement_expiry_unix_seconds: Option<i64>,
    pub compensation_expiry_unix_seconds: Option<i64>,
    pub error: Option<String>,
    pub current_install_dispatched: bool,
}

impl RemoveReplicaProgress {
    pub fn validate(&self) -> Result<(), String> {
        if self.attempt_id.is_empty() {
            return Err("remove-replica progress has no attempt ID".to_string());
        }
        if self
            .error
            .as_ref()
            .is_some_and(|error| error.len() > MAX_LIFECYCLE_ERROR_BYTES)
        {
            return Err("remove-replica progress error exceeds the protocol bound".to_string());
        }
        if self
            .commit_observed_unix_seconds
            .is_some_and(|value| value <= 0)
            || self
                .retirement_expiry_unix_seconds
                .is_some_and(|value| value <= 0)
            || self
                .compensation_expiry_unix_seconds
                .is_some_and(|value| value <= 0)
        {
            return Err("remove-replica progress contains an invalid expiry".to_string());
        }
        if self.commit_observed {
            let commit_time = self
                .commit_observed_unix_seconds
                .ok_or_else(|| "committed removal progress has no commit time".to_string())?;
            let retirement_expiry = self
                .retirement_expiry_unix_seconds
                .ok_or_else(|| "committed removal progress has no retirement expiry".to_string())?;
            if retirement_expiry < commit_time {
                return Err(
                    "remove-replica retirement expiry precedes commit observation".to_string(),
                );
            }
            if self.compensation_expiry_unix_seconds.is_some() {
                return Err("committed removal progress carries a compensation expiry".to_string());
            }
        } else if self.commit_observed_unix_seconds.is_some()
            || self.retirement_expiry_unix_seconds.is_some()
            || self.connection_absent
            || self.target_retirement != TargetRetirementObservation::NotAttempted
        {
            return Err("pre-commit removal progress carries post-commit evidence".to_string());
        }
        match self.phase {
            RemoveReplicaCoordinatorPhase::Validating
            | RemoveReplicaCoordinatorPhase::InstallingCatchUpConfiguration
            | RemoveReplicaCoordinatorPhase::WaitingForCatchUpQuorum
                if self.commit_observed =>
            {
                return Err("pre-commit removal phase claims commit".to_string());
            }
            RemoveReplicaCoordinatorPhase::RemovingConnection
            | RemoveReplicaCoordinatorPhase::RetiringTarget
            | RemoveReplicaCoordinatorPhase::Attesting
                if !self.commit_observed =>
            {
                return Err("post-commit removal phase has no commit evidence".to_string());
            }
            RemoveReplicaCoordinatorPhase::Compensating
                if self.commit_observed || self.compensation_expiry_unix_seconds.is_none() =>
            {
                return Err("removal compensation progress is inconsistent".to_string());
            }
            _ => {}
        }
        if self.current_install_dispatched
            && matches!(
                self.phase,
                RemoveReplicaCoordinatorPhase::Validating
                    | RemoveReplicaCoordinatorPhase::InstallingCatchUpConfiguration
                    | RemoveReplicaCoordinatorPhase::WaitingForCatchUpQuorum
            )
        {
            return Err(
                "remove-replica current-install evidence precedes its coordinator phase"
                    .to_string(),
            );
        }
        if self.target_retirement == TargetRetirementObservation::Completed
            && !self.connection_absent
        {
            return Err(
                "completed target retirement precedes exact connection absence".to_string(),
            );
        }
        Ok(())
    }

    pub fn validate_terminal(&self, result: RemoveReplicaTerminalResult) -> Result<(), String> {
        self.validate()?;
        match result {
            RemoveReplicaTerminalResult::CommittedClean
                if !self.commit_observed
                    || !self.connection_absent
                    || self.phase != RemoveReplicaCoordinatorPhase::Attesting
                    || self.target_retirement != TargetRetirementObservation::Completed =>
            {
                Err("committed-clean removal result lacks exact clean evidence".to_string())
            }
            RemoveReplicaTerminalResult::CommittedDegraded
                if !self.commit_observed
                    || !self.connection_absent
                    || self.phase != RemoveReplicaCoordinatorPhase::Attesting
                    || !matches!(
                        self.target_retirement,
                        TargetRetirementObservation::Unavailable
                            | TargetRetirementObservation::Stale
                            | TargetRetirementObservation::Failed
                    ) =>
            {
                Err("committed-degraded removal result lacks degraded evidence".to_string())
            }
            RemoveReplicaTerminalResult::Compensated
                if self.commit_observed
                    || self.phase != RemoveReplicaCoordinatorPhase::Compensating
                    || self.compensation_expiry_unix_seconds.is_none()
                    || self.current_install_dispatched =>
            {
                Err(
                    "compensated removal result lacks safe undispatched restoration evidence"
                        .to_string(),
                )
            }
            RemoveReplicaTerminalResult::CompensationIncomplete
                if self.commit_observed
                    || self.phase != RemoveReplicaCoordinatorPhase::Compensating
                    || self.compensation_expiry_unix_seconds.is_none()
                    || self.error.is_none() =>
            {
                Err("incomplete removal compensation lacks bounded failure evidence".to_string())
            }
            _ => Ok(()),
        }
    }
}

pub fn normalize_remove_error(error: &str) -> String {
    if error.len() <= MAX_LIFECYCLE_ERROR_BYTES {
        return error.to_string();
    }
    let mut boundary = MAX_LIFECYCLE_ERROR_BYTES;
    while !error.is_char_boundary(boundary) {
        boundary -= 1;
    }
    error[..boundary].to_string()
}

pub trait RemoveReplicaClock: Send + Sync {
    fn unix_seconds(&self) -> i64;
}

#[derive(Debug, Default)]
pub struct SystemRemoveReplicaClock;

impl RemoveReplicaClock for SystemRemoveReplicaClock {
    fn unix_seconds(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| {
                i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
            })
    }
}

#[derive(Debug, Clone)]
pub struct ManualRemoveReplicaClock {
    unix_seconds: Arc<AtomicI64>,
}

impl ManualRemoveReplicaClock {
    pub fn new(unix_seconds: i64) -> Self {
        Self {
            unix_seconds: Arc::new(AtomicI64::new(unix_seconds)),
        }
    }

    pub fn set(&self, unix_seconds: i64) {
        self.unix_seconds.store(unix_seconds, Ordering::SeqCst);
    }

    pub fn advance(&self, seconds: i64) -> i64 {
        self.unix_seconds.fetch_add(seconds, Ordering::SeqCst) + seconds
    }
}

impl RemoveReplicaClock for ManualRemoveReplicaClock {
    fn unix_seconds(&self) -> i64 {
        self.unix_seconds.load(Ordering::SeqCst)
    }
}

fn validate_endpoint(name: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err(format!("remove-replica {name} endpoint is missing"));
    }
    Endpoint::from_shared(value.to_string())
        .map(|_| ())
        .map_err(|_| format!("remove-replica {name} endpoint is invalid"))
}

fn validate_target_observability(intent: &RemoveReplicaIntent) -> Result<(), String> {
    let present = [
        intent.expected_target_agent_generation.is_some(),
        intent.target_control_address.is_some(),
        intent.target_replicator_address.is_some(),
        intent.target_lifecycle_peer_protocol_version.is_some(),
    ];
    let all_present = present.iter().all(|value| *value);
    let all_absent = present.iter().all(|value| !*value);
    if !all_present && !all_absent {
        return Err("remove-replica target observability is partial".to_string());
    }
    if intent.mode == RemoveReplicaMode::ScaleDown && !all_present {
        return Err("scale-down removal requires exact target observability".to_string());
    }
    if all_present {
        validate_endpoint(
            "target control",
            intent.target_control_address.as_deref().unwrap_or_default(),
        )?;
        validate_endpoint(
            "target replicator",
            intent
                .target_replicator_address
                .as_deref()
                .unwrap_or_default(),
        )?;
        if intent.target_lifecycle_peer_protocol_version
            != Some(REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION)
        {
            return Err("remove-replica target lifecycle peer version is unsupported".to_string());
        }
    }
    Ok(())
}

fn validate_descriptor(name: &str, descriptor: &ConfigurationDescriptor) -> Result<(), String> {
    let total_members = descriptor.members.len() + 1;
    let expected_write_quorum = total_members as u32 / 2 + 1;
    if descriptor.write_quorum != expected_write_quorum {
        return Err(format!(
            "remove-replica {name} configuration has write quorum {}, expected {}",
            descriptor.write_quorum, expected_write_quorum
        ));
    }
    let mut ids = HashSet::new();
    let mut instances = HashSet::new();
    for member in &descriptor.members {
        if member.id <= 0
            || !ids.insert(member.id)
            || member.instance_id.as_str().is_empty()
            || !instances.insert(member.instance_id.clone())
        {
            return Err(format!(
                "remove-replica {name} configuration has invalid or duplicate identity"
            ));
        }
        if member.role != Role::ActiveSecondary
            || member.must_catch_up
            || !matches!(member.progress, ConfigurationProgressSource::Frozen { .. })
        {
            return Err(format!(
                "remove-replica {name} configuration member {} is not stable",
                member.id
            ));
        }
        validate_endpoint(
            &format!("{name} configuration member {} replicator", member.id),
            &member.replicator_address,
        )?;
    }
    Ok(())
}

fn validate_configuration_relationship(intent: &RemoveReplicaIntent) -> Result<(), String> {
    for descriptor in [
        &intent.previous_configuration,
        &intent.reduced_catch_up_configuration,
        &intent.reduced_current_configuration,
    ] {
        if descriptor.members.iter().any(|member| {
            member.id == intent.primary_replica_id
                || member.instance_id == intent.primary_instance_id
        }) {
            return Err(
                "remove-replica primary appears in a remote configuration descriptor".to_string(),
            );
        }
    }
    let targets = intent
        .previous_configuration
        .members
        .iter()
        .filter(|member| {
            member.id == intent.target_replica_id && member.instance_id == intent.target_instance_id
        })
        .collect::<Vec<_>>();
    if targets.len() != 1 || targets[0].role != Role::ActiveSecondary {
        return Err("remove-replica target is not one exact stable previous secondary".to_string());
    }
    if intent.mode == RemoveReplicaMode::ScaleDown && targets[0].status != ReplicaStatus::Up {
        return Err("scale-down removal target is not up".to_string());
    }
    if intent.reduced_catch_up_configuration != intent.reduced_current_configuration {
        return Err("remove-replica reduced catch-up and current descriptors differ".to_string());
    }
    if intent.previous_configuration.members.len()
        != intent.reduced_current_configuration.members.len() + 1
    {
        return Err("remove-replica descriptors do not differ by exactly one member".to_string());
    }
    if intent
        .reduced_current_configuration
        .members
        .iter()
        .any(|member| {
            member.id == intent.target_replica_id || member.instance_id == intent.target_instance_id
        })
    {
        return Err("remove-replica target remains in a reduced descriptor".to_string());
    }
    let previous_members = intent
        .previous_configuration
        .members
        .iter()
        .map(|member| (member.id, member))
        .collect::<HashMap<_, _>>();
    for retained in &intent.reduced_current_configuration.members {
        if previous_members.get(&retained.id).copied() != Some(retained) {
            return Err(format!(
                "remove-replica retained member {} changed",
                retained.id
            ));
        }
    }
    Ok(())
}

fn validate_limits(intent: &RemoveReplicaIntent) -> Result<(), String> {
    let previous_total = intent.previous_configuration.members.len() + 1;
    let retained_total = intent.reduced_current_configuration.members.len() + 1;
    if intent.required_write_quorum == 0
        || intent.required_write_quorum != intent.previous_configuration.write_quorum
        || intent.required_write_quorum as usize > retained_total
        || intent.minimum_committed_replicas == 0
        || intent.minimum_committed_replicas as usize > retained_total
    {
        return Err("remove-replica quorum or minimum constraint is invalid".to_string());
    }
    let retained_up = intent
        .reduced_current_configuration
        .members
        .iter()
        .filter(|member| member.status == ReplicaStatus::Up)
        .count()
        + 1;
    if retained_up < intent.required_write_quorum as usize
        || retained_up < intent.reduced_current_configuration.write_quorum as usize
        || previous_total != retained_total + 1
    {
        return Err("remove-replica retained set cannot satisfy required quorums".to_string());
    }
    if intent.overall_deadline_unix_seconds <= 0
        || intent.compensation_grace_seconds != REMOVE_REPLICA_COMPENSATION_GRACE_SECONDS
        || intent.call_timeout_seconds != REMOVE_REPLICA_CALL_TIMEOUT_SECONDS
        || intent.target_retirement_timeout_seconds != REMOVE_REPLICA_RETIREMENT_TIMEOUT_SECONDS
        || intent.call_timeout_seconds > intent.compensation_grace_seconds
        || intent.call_timeout_seconds > intent.target_retirement_timeout_seconds
        || intent.compensation_deadline_cap_unix_seconds
            != intent
                .overall_deadline_unix_seconds
                .checked_add(intent.compensation_grace_seconds)
                .ok_or_else(|| "remove-replica compensation deadline overflows".to_string())?
    {
        return Err("remove-replica deadline budget is invalid".to_string());
    }
    Ok(())
}

fn optional_generation(value: &Option<AgentGeneration>) -> &str {
    value.as_ref().map_or("none", AgentGeneration::as_str)
}

fn optional_text(value: Option<&str>) -> &str {
    value.unwrap_or("none")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::replica_lifecycle::ConfigurationMemberDescriptor;

    type IntentMutation = Box<dyn Fn(&mut RemoveReplicaIntent)>;

    fn generation(value: char) -> AgentGeneration {
        AgentGeneration::parse(value.to_string().repeat(32)).unwrap()
    }

    fn member(
        id: ReplicaId,
        instance: &str,
        status: ReplicaStatus,
    ) -> ConfigurationMemberDescriptor {
        ConfigurationMemberDescriptor {
            id,
            instance_id: ReplicaInstanceId::new(instance),
            role: Role::ActiveSecondary,
            status,
            replicator_address: format!("http://replica-{id}:7001"),
            must_catch_up: false,
            progress: ConfigurationProgressSource::Frozen {
                current_progress: id * 10,
                catch_up_capability: id * 10,
            },
        }
    }

    fn intent() -> RemoveReplicaIntent {
        let reduced = ConfigurationDescriptor {
            members: vec![member(3, "retained", ReplicaStatus::Up)],
            write_quorum: 2,
        };
        let mut intent = RemoveReplicaIntent {
            protocol_version: REMOVE_REPLICA_INTENT_PROTOCOL_VERSION,
            operation_id: "remove-operation".to_string(),
            action_id: "remove-action-1".to_string(),
            attempt_number: 1,
            attempt_id: "attempt-1".to_string(),
            input_signature: String::new(),
            mode: RemoveReplicaMode::ScaleDown,
            epoch: Epoch::new(4, 9),
            primary_replica_id: 1,
            primary_instance_id: ReplicaInstanceId::new("primary"),
            primary_agent_generation: generation('a'),
            primary_agent_control_version: AgentControlVersion::new(7),
            primary_control_address: "http://primary:7000".to_string(),
            primary_replicator_address: "http://primary:7001".to_string(),
            target_replica_id: 2,
            target_instance_id: ReplicaInstanceId::new("target"),
            expected_target_pod_uid: "target".to_string(),
            target_pod_name: "set-2".to_string(),
            expected_target_agent_generation: Some(generation('b')),
            target_control_address: Some("http://target:7000".to_string()),
            target_replicator_address: Some("http://target:7001".to_string()),
            target_lifecycle_peer_protocol_version: Some(REPLICA_LIFECYCLE_PEER_PROTOCOL_VERSION),
            previous_configuration: ConfigurationDescriptor {
                members: vec![
                    member(2, "target", ReplicaStatus::Up),
                    member(3, "retained", ReplicaStatus::Up),
                ],
                write_quorum: 2,
            },
            reduced_catch_up_configuration: reduced.clone(),
            reduced_current_configuration: reduced,
            required_write_quorum: 2,
            minimum_committed_replicas: 2,
            maximum_pre_commit_attempts: MAX_REMOVE_REPLICA_PRE_COMMIT_ATTEMPTS,
            overall_deadline_unix_seconds: 1_600,
            compensation_grace_seconds: REMOVE_REPLICA_COMPENSATION_GRACE_SECONDS,
            compensation_deadline_cap_unix_seconds: 1_630,
            call_timeout_seconds: REMOVE_REPLICA_CALL_TIMEOUT_SECONDS,
            target_retirement_timeout_seconds: REMOVE_REPLICA_RETIREMENT_TIMEOUT_SECONDS,
        };
        intent.input_signature = intent.signature();
        intent
    }

    fn resign(intent: &mut RemoveReplicaIntent) {
        intent.input_signature = intent.signature();
    }

    #[test]
    fn valid_modes_and_status_projections_are_exact() {
        let scale_down = intent();
        scale_down.validate().unwrap();
        assert_eq!(
            scale_down.previous_status().mode,
            ReplicaConfigurationMode::Current
        );
        assert_eq!(
            scale_down.reduced_catch_up_status().mode,
            ReplicaConfigurationMode::CatchUp
        );
        let current = scale_down.reduced_current_status();
        assert_eq!(current.mode, ReplicaConfigurationMode::Current);
        assert_eq!(current.members.len(), 1);
        assert_eq!(current.members[0].id, 3);
        assert_eq!(scale_down.retirement_expiry(1_550).unwrap(), 1_600);
        assert_eq!(scale_down.compensation_expiry(1_610).unwrap(), 1_630);

        let mut force = scale_down;
        force.mode = RemoveReplicaMode::Force;
        force.expected_target_agent_generation = None;
        force.target_control_address = None;
        force.target_replicator_address = None;
        force.target_lifecycle_peer_protocol_version = None;
        force.previous_configuration.members[0].status = ReplicaStatus::Down;
        resign(&mut force);
        force.validate().unwrap();
    }

    #[test]
    fn invalid_intents_are_rejected_without_mutation() {
        let cases: Vec<(&str, IntentMutation)> = vec![
            ("version", Box::new(|value| value.protocol_version = 2)),
            (
                "target-primary",
                Box::new(|value| value.target_replica_id = value.primary_replica_id),
            ),
            (
                "duplicate-target",
                Box::new(|value| {
                    value
                        .previous_configuration
                        .members
                        .push(value.previous_configuration.members[0].clone());
                    value.previous_configuration.write_quorum = 3;
                }),
            ),
            (
                "target-outside-previous",
                Box::new(|value| value.target_replica_id = 4),
            ),
            (
                "more-than-one-difference",
                Box::new(|value| {
                    value.reduced_catch_up_configuration.members.clear();
                    value.reduced_catch_up_configuration.write_quorum = 1;
                    value.reduced_current_configuration.members.clear();
                    value.reduced_current_configuration.write_quorum = 1;
                }),
            ),
            (
                "target-role",
                Box::new(|value| {
                    value.previous_configuration.members[0].role = Role::IdleSecondary
                }),
            ),
            (
                "empty-uid",
                Box::new(|value| value.expected_target_pod_uid.clear()),
            ),
            (
                "mismatched-uid",
                Box::new(|value| value.expected_target_pod_uid = "other".to_string()),
            ),
            (
                "invalid-primary-endpoint",
                Box::new(|value| value.primary_control_address = "not a URI".to_string()),
            ),
            (
                "invalid-target-endpoint",
                Box::new(|value| value.target_control_address = Some("not a URI".to_string())),
            ),
            (
                "invalid-quorum",
                Box::new(|value| value.required_write_quorum = 3),
            ),
            (
                "invalid-minimum",
                Box::new(|value| value.minimum_committed_replicas = 3),
            ),
            (
                "malformed-deadline",
                Box::new(|value| value.compensation_deadline_cap_unix_seconds = 1_631),
            ),
            ("attempt-limit", Box::new(|value| value.attempt_number = 4)),
            (
                "partial-force-observability",
                Box::new(|value| {
                    value.mode = RemoveReplicaMode::Force;
                    value.target_control_address = None;
                }),
            ),
        ];
        for (name, mutate) in cases {
            let mut invalid = intent();
            mutate(&mut invalid);
            resign(&mut invalid);
            assert!(invalid.validate().is_err(), "{name} was accepted");
        }
    }

    #[test]
    fn supplied_signature_must_match_and_every_semantic_mutation_changes_it() {
        let original = intent();
        let signature = original.signature();
        let mutations: Vec<IntentMutation> = vec![
            Box::new(|value| value.protocol_version += 1),
            Box::new(|value| value.operation_id.push('x')),
            Box::new(|value| value.action_id.push('x')),
            Box::new(|value| value.attempt_number += 1),
            Box::new(|value| value.attempt_id.push('x')),
            Box::new(|value| value.mode = RemoveReplicaMode::Force),
            Box::new(|value| value.epoch.data_loss_number += 1),
            Box::new(|value| value.epoch.configuration_number += 1),
            Box::new(|value| value.primary_replica_id += 10),
            Box::new(|value| value.primary_instance_id = ReplicaInstanceId::new("other-primary")),
            Box::new(|value| value.primary_agent_generation = generation('c')),
            Box::new(|value| value.primary_agent_control_version = AgentControlVersion::new(8)),
            Box::new(|value| value.primary_control_address.push_str("/changed")),
            Box::new(|value| value.primary_replicator_address.push_str("/changed")),
            Box::new(|value| value.target_replica_id += 10),
            Box::new(|value| value.target_instance_id = ReplicaInstanceId::new("other-target")),
            Box::new(|value| value.expected_target_pod_uid.push('x')),
            Box::new(|value| value.target_pod_name.push('x')),
            Box::new(|value| value.expected_target_agent_generation = Some(generation('d'))),
            Box::new(|value| value.target_control_address = Some("http://target:7010".to_string())),
            Box::new(|value| {
                value.target_replicator_address = Some("http://target:7011".to_string())
            }),
            Box::new(|value| value.target_lifecycle_peer_protocol_version = Some(3)),
            Box::new(|value| value.previous_configuration.members[0].id += 10),
            Box::new(|value| {
                value.previous_configuration.members[0].instance_id =
                    ReplicaInstanceId::new("changed-target")
            }),
            Box::new(|value| value.previous_configuration.members[0].role = Role::IdleSecondary),
            Box::new(|value| value.previous_configuration.members[0].status = ReplicaStatus::Down),
            Box::new(|value| {
                value.previous_configuration.members[0]
                    .replicator_address
                    .push_str("/changed")
            }),
            Box::new(|value| value.previous_configuration.members[0].must_catch_up = true),
            Box::new(|value| {
                value.previous_configuration.members[0].progress =
                    ConfigurationProgressSource::BuildCopyLsn
            }),
            Box::new(|value| value.previous_configuration.write_quorum += 1),
            Box::new(|value| {
                value.reduced_catch_up_configuration.members[0]
                    .replicator_address
                    .push_str("/changed")
            }),
            Box::new(|value| value.reduced_catch_up_configuration.write_quorum += 1),
            Box::new(|value| {
                value.reduced_current_configuration.members[0].progress =
                    ConfigurationProgressSource::Frozen {
                        current_progress: 999,
                        catch_up_capability: 999,
                    }
            }),
            Box::new(|value| value.reduced_current_configuration.write_quorum += 1),
            Box::new(|value| value.required_write_quorum += 1),
            Box::new(|value| value.minimum_committed_replicas += 1),
            Box::new(|value| value.maximum_pre_commit_attempts += 1),
            Box::new(|value| value.overall_deadline_unix_seconds += 1),
            Box::new(|value| value.compensation_grace_seconds += 1),
            Box::new(|value| value.compensation_deadline_cap_unix_seconds += 1),
            Box::new(|value| value.call_timeout_seconds += 1),
            Box::new(|value| value.target_retirement_timeout_seconds += 1),
        ];
        for mutate in mutations {
            let mut changed = original.clone();
            mutate(&mut changed);
            assert_ne!(changed.signature(), signature);
        }

        let mut stale = original;
        stale.target_pod_name.push('x');
        assert!(stale.validate().unwrap_err().contains("signature"));
    }

    #[test]
    fn progress_and_terminal_invariants_are_strict() {
        let clean = RemoveReplicaProgress {
            phase: RemoveReplicaCoordinatorPhase::Attesting,
            attempt_id: "attempt-1".to_string(),
            commit_observed: true,
            commit_observed_unix_seconds: Some(100),
            connection_absent: true,
            target_retirement: TargetRetirementObservation::Completed,
            retirement_expiry_unix_seconds: Some(160),
            compensation_expiry_unix_seconds: None,
            error: None,
            current_install_dispatched: true,
        };
        clean
            .validate_terminal(RemoveReplicaTerminalResult::CommittedClean)
            .unwrap();
        intent()
            .validate_terminal_progress(&clean, RemoveReplicaTerminalResult::CommittedClean)
            .unwrap();
        assert!(
            clean
                .validate_terminal(RemoveReplicaTerminalResult::CommittedDegraded)
                .is_err()
        );
        let mut wrong_terminal_phase = clean.clone();
        wrong_terminal_phase.phase = RemoveReplicaCoordinatorPhase::RemovingConnection;
        assert!(
            wrong_terminal_phase
                .validate_terminal(RemoveReplicaTerminalResult::CommittedClean)
                .is_err()
        );
        let mut committed_with_compensation = clean.clone();
        committed_with_compensation.compensation_expiry_unix_seconds = Some(130);
        assert!(committed_with_compensation.validate().is_err());

        let mut compensated = clean;
        compensated.phase = RemoveReplicaCoordinatorPhase::Compensating;
        compensated.commit_observed = false;
        compensated.commit_observed_unix_seconds = None;
        compensated.connection_absent = false;
        compensated.target_retirement = TargetRetirementObservation::NotAttempted;
        compensated.retirement_expiry_unix_seconds = None;
        compensated.compensation_expiry_unix_seconds = Some(130);
        compensated.current_install_dispatched = false;
        compensated
            .validate_terminal(RemoveReplicaTerminalResult::Compensated)
            .unwrap();
        let mut dispatched_compensation = compensated.clone();
        dispatched_compensation.current_install_dispatched = true;
        assert!(
            dispatched_compensation
                .validate_terminal(RemoveReplicaTerminalResult::Compensated)
                .unwrap_err()
                .contains("undispatched")
        );
        compensated.error = Some("restoration not observed".to_string());
        compensated
            .validate_terminal(RemoveReplicaTerminalResult::CompensationIncomplete)
            .unwrap();
    }

    #[test]
    fn bounded_errors_and_manual_clock_are_deterministic() {
        let error = "é".repeat(MAX_LIFECYCLE_ERROR_BYTES);
        let normalized = normalize_remove_error(&error);
        assert!(normalized.len() <= MAX_LIFECYCLE_ERROR_BYTES);
        assert!(normalized.is_char_boundary(normalized.len()));

        let clock = ManualRemoveReplicaClock::new(100);
        assert_eq!(clock.unix_seconds(), 100);
        assert_eq!(clock.advance(10), 110);
        clock.set(200);
        assert_eq!(clock.unix_seconds(), 200);
    }
}
