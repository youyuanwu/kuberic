use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

macro_rules! string_id {
    ($name:ident) => {
        #[derive(
            Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
        )]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }

            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

string_id!(ResourceUid);
string_id!(PodUid);
string_id!(PvcUid);
string_id!(ReplicaInstanceId);
string_id!(AgentGeneration);
string_id!(ProcessSessionId);
string_id!(ConfigurationId);
string_id!(TransitionId);
string_id!(ProvisioningId);
string_id!(InitializationId);
string_id!(OperationId);

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(transparent)]
pub struct ReplicaId(i64);

impl ReplicaId {
    pub const fn new(value: i64) -> Self {
        Self(value)
    }

    pub const fn value(self) -> i64 {
        self.0
    }
}

impl fmt::Display for ReplicaId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, Default,
)]
#[serde(rename_all = "camelCase")]
pub struct Epoch {
    pub data_loss_number: i64,
    pub configuration_number: i64,
}

impl Epoch {
    pub const fn new(data_loss_number: i64, configuration_number: i64) -> Self {
        Self {
            data_loss_number,
            configuration_number,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReplicaRole {
    Primary,
    ActiveSecondary,
    IdleSecondary,
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AccessStatus {
    Granted,
    ReconfigurationPending,
    NotPrimary,
    NoWriteQuorum,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaIdentity {
    pub replica_id: ReplicaId,
    pub instance_id: ReplicaInstanceId,
    pub agent_generation: AgentGeneration,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationMember {
    pub identity: ReplicaIdentity,
    pub role: ReplicaRole,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurationDescriptor {
    pub configuration_id: ConfigurationId,
    pub epoch: Epoch,
    pub primary_id: ReplicaId,
    pub members: Vec<ConfigurationMember>,
    pub write_quorum: u32,
}

impl ConfigurationDescriptor {
    pub fn new(
        epoch: Epoch,
        primary_id: ReplicaId,
        mut members: Vec<ConfigurationMember>,
        write_quorum: u32,
    ) -> Self {
        members.sort_by_key(|member| member.identity.replica_id);
        let configuration_id = Self::calculate_id(epoch, primary_id, &members, write_quorum);
        Self {
            configuration_id,
            epoch,
            primary_id,
            members,
            write_quorum,
        }
    }

    pub fn expected_id(&self) -> ConfigurationId {
        Self::calculate_id(
            self.epoch,
            self.primary_id,
            &self.members,
            self.write_quorum,
        )
    }

    fn calculate_id(
        epoch: Epoch,
        primary_id: ReplicaId,
        members: &[ConfigurationMember],
        write_quorum: u32,
    ) -> ConfigurationId {
        let mut canonical_members = members.to_vec();
        canonical_members.sort_by_key(|member| member.identity.replica_id);
        let mut hasher = Sha256::new();
        hasher.update(b"kuberic-configuration-v1");
        hasher.update(epoch.data_loss_number.to_be_bytes());
        hasher.update(epoch.configuration_number.to_be_bytes());
        hasher.update(primary_id.value().to_be_bytes());
        hasher.update(write_quorum.to_be_bytes());
        hasher.update((canonical_members.len() as u64).to_be_bytes());
        for member in canonical_members {
            hasher.update(member.identity.replica_id.value().to_be_bytes());
            update_digest_string(&mut hasher, member.identity.instance_id.as_str());
            update_digest_string(&mut hasher, member.identity.agent_generation.as_str());
            hasher.update([role_tag(member.role)]);
        }
        ConfigurationId::new(format!("cfg-{}", format_digest(hasher.finalize())))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EffectivePolicy {
    pub replica_set_size: u32,
    pub write_quorum: u32,
    pub read_quorum: u32,
    pub failover_delay_seconds: u64,
}

impl EffectivePolicy {
    pub fn fixed(replica_set_size: u32, failover_delay_seconds: u64) -> Option<Self> {
        if replica_set_size == 0 {
            return None;
        }
        let write_quorum = replica_set_size / 2 + 1;
        let read_quorum = replica_set_size - write_quorum + 1;
        Some(Self {
            replica_set_size,
            write_quorum,
            read_quorum,
            failover_delay_seconds,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedTopology {
    pub configuration: ConfigurationDescriptor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProvisioningKind {
    Replacement,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProvisioningIntent {
    pub provisioning_id: ProvisioningId,
    pub kind: ProvisioningKind,
    pub resource_uid: ResourceUid,
    pub replica_id: ReplicaId,
    pub instance_id: ReplicaInstanceId,
    pub pod_uid: PodUid,
    pub pvc_uid: PvcUid,
    pub initialization_id: InitializationId,
    pub assigned_agent_generation: AgentGeneration,
    pub operation_id: OperationId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TransitionKind {
    Bootstrap,
    Replacement,
    Failover,
}

impl TransitionKind {
    pub const fn as_tag(self) -> &'static str {
        match self {
            Self::Bootstrap => "bootstrap",
            Self::Replacement => "replacement",
            Self::Failover => "failover",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TransitionIntent {
    pub transition_id: TransitionId,
    pub kind: TransitionKind,
    pub spec_generation: u64,
    pub effective_policy: EffectivePolicy,
    pub previous_configuration_id: Option<ConfigurationId>,
    pub current_configuration: ConfigurationDescriptor,
    pub started_at_unix_seconds: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ConditionStatus {
    True,
    False,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StatusCondition {
    pub type_: String,
    pub status: ConditionStatus,
    pub reason: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AcceptedStatus {
    pub initialized: bool,
    pub observed_generation: u64,
    pub topology: Option<AcceptedTopology>,
    pub provisioning: Option<ProvisioningIntent>,
    pub transition: Option<TransitionIntent>,
    pub conditions: Vec<StatusCondition>,
}

impl AcceptedStatus {
    pub fn with_condition(mut self, condition: StatusCondition) -> Self {
        self.conditions
            .retain(|existing| existing.type_ != condition.type_);
        self.conditions.push(condition);
        self
    }

    pub fn without_condition(mut self, type_: &str) -> Self {
        self.conditions.retain(|existing| existing.type_ != type_);
        self
    }
}

pub fn derive_initialization_id(
    resource_uid: &ResourceUid,
    replica_id: ReplicaId,
    pod_uid: &PodUid,
    pvc_uid: &PvcUid,
) -> InitializationId {
    InitializationId::new(format!(
        "init-{}",
        digest_parts(&[
            resource_uid.as_str(),
            &replica_id.to_string(),
            pod_uid.as_str(),
            pvc_uid.as_str(),
        ])
    ))
}

pub fn derive_agent_generation(initialization_id: &InitializationId) -> AgentGeneration {
    AgentGeneration::new(digest_parts(&["agent", initialization_id.as_str()]))
}

pub fn derive_transition_id(
    resource_uid: &ResourceUid,
    kind: TransitionKind,
    configuration_id: &ConfigurationId,
) -> TransitionId {
    TransitionId::new(format!(
        "transition-{}",
        digest_parts(&[
            resource_uid.as_str(),
            kind.as_tag(),
            configuration_id.as_str(),
        ])
    ))
}

pub fn duplicate_replica_ids(members: &[ConfigurationMember]) -> BTreeSet<ReplicaId> {
    let mut seen = BTreeSet::new();
    let mut duplicates = BTreeSet::new();
    for member in members {
        if !seen.insert(member.identity.replica_id) {
            duplicates.insert(member.identity.replica_id);
        }
    }
    duplicates
}

fn digest_parts(parts: &[&str]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format_digest(hasher.finalize())
}

fn update_digest_string(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

const fn role_tag(role: ReplicaRole) -> u8 {
    match role {
        ReplicaRole::Primary => 1,
        ReplicaRole::ActiveSecondary => 2,
        ReplicaRole::IdleSecondary => 3,
        ReplicaRole::None => 4,
    }
}

fn format_digest(digest: impl AsRef<[u8]>) -> String {
    use std::fmt::Write;

    let mut output = String::with_capacity(digest.as_ref().len() * 2);
    for byte in digest.as_ref() {
        write!(&mut output, "{byte:02x}").expect("writing to a string cannot fail");
    }
    output
}
