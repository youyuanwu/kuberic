use super::*;

pub const ORDINARY_ACTION_FORMAT: u32 = 19;
pub const MAX_REMOTE_SELECTIONS: usize = 256;
pub const MAX_REMOTE_GENERATIONS: usize = 64;

/// Historical eligibility only: no progress, Ready or successor authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetainedMemberQualification {
    pub predecessor_process_session_id: ProcessSessionId,
    pub predecessor_creation_id: ReplicatorCreationId,
    pub admitted_operation_id: OperationId,
    pub admitted_revision: u64,
    pub configuration: ConfigurationDescriptor,
}

/// An instruction to the executable to exclude this exact physical owner.
/// It is neither successful Cleanup nor proof that the process has exited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RetainedRecoveryExitNotice {
    pub fault_operation_id: OperationId,
    pub fault_revision: u64,
    pub process_id: u32,
    pub process_lifetime: String,
    pub history: HistoryContext,
    pub storage: ReplicaStorageIncarnationContext,
    pub qualification: RetainedMemberQualification,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaPeerDescription {
    pub identity: ReplicaIdentity,
    pub replication_address: String,
    pub process_session_id: ProcessSessionId,
    pub storage_incarnation: ReplicaStorageIncarnationContext,
    pub role: ReplicaRole,
    pub current_progress: i64,
    pub catch_up_capability: i64,
    pub is_up: bool,
    pub is_ready: bool,
    pub must_catchup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaConfigurationScope {
    pub current: ConfigurationDescriptor,
    pub previous: Option<ConfigurationDescriptor>,
    pub replicas: Vec<ReplicaPeerDescription>,
}

impl ReplicaConfigurationScope {
    pub(crate) fn fresh_idle_replacement(
        &self,
        storage: &ReplicaStorageIncarnationContext,
        process: &ProcessSessionId,
    ) -> bool {
        self.previous.is_none()
            && self.current.primary_id != storage.replica.replica_id
            && self
                .current
                .members
                .iter()
                .filter(|member| member.identity.replica_id == storage.replica.replica_id)
                .count()
                == 1
            && self.current.members.iter().any(|member| {
                member.identity.replica_id == storage.replica.replica_id
                    && member.identity != storage.replica
                    && member.role == ReplicaRole::ActiveSecondary
                    && self.replicas.iter().any(|peer| {
                        peer.identity == member.identity
                            && peer.storage_incarnation != *storage
                            && match (&peer.storage_incarnation.storage, &storage.storage) {
                                (
                                    ReplicaStorageIdentity::Persistent { pvc_uid: old, .. },
                                    ReplicaStorageIdentity::Persistent { pvc_uid: fresh, .. },
                                ) => old != fresh,
                                (old, fresh) => old != fresh,
                            }
                            && peer.process_session_id != *process
                            && peer.role == ReplicaRole::ActiveSecondary
                            && !peer.is_up
                            && !peer.is_ready
                            && !peer.must_catchup
                            && peer.current_progress == super::INVALID_LSN
                            && peer.catch_up_capability == super::INVALID_LSN
                    })
            })
    }

    pub(crate) fn ordinary(&self) -> PublicConfiguration {
        PublicConfiguration {
            current: self.current.clone(),
            previous: self.previous.clone(),
            replicas: self
                .replicas
                .iter()
                .map(|peer| PublicReplicaDescriptor {
                    identity: peer.identity.clone(),
                    replication_address: peer.replication_address.clone(),
                    process_session_id: peer.process_session_id.clone(),
                    storage_incarnation: Some(peer.storage_incarnation.clone()),
                    role: peer.role,
                    current_progress: peer.current_progress,
                    catch_up_capability: peer.catch_up_capability,
                    is_up: peer.is_up,
                    is_ready: peer.is_ready,
                    must_catchup: peer.must_catchup,
                })
                .collect(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaScope {
    pub history: HistoryContext,
    pub storage: ReplicaStorageIncarnationContext,
    pub process_session_id: ProcessSessionId,
    pub creation_id: ReplicatorCreationId,
    pub epoch: Epoch,
    pub configuration: ReplicaConfigurationScope,
}

impl ReplicaScope {
    pub fn compatible_with(&self, next: &Self) -> bool {
        self.history == next.history
            && self.storage == next.storage
            && self.process_session_id == next.process_session_id
            && self.creation_id == next.creation_id
            && self.epoch.data_loss_number == next.epoch.data_loss_number
            && self.epoch.configuration_number <= next.epoch.configuration_number
            && (self.configuration.current.primary_id == next.configuration.current.primary_id
                || self.epoch.configuration_number < next.epoch.configuration_number)
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.history.validate()?;
        self.storage.validate()?;
        self.configuration.ordinary().validate_descriptors()?;
        for peer in &self.configuration.ordinary().replicas {
            peer.validate()?;
        }
        if self.process_session_id.is_empty()
            || self.creation_id.is_empty()
            || self.epoch.configuration_number < 0
            || self.configuration.current.epoch != self.epoch
            || self.history.data_loss_number != self.epoch.data_loss_number
        {
            return Err("replica scope has incomplete or inconsistent identity");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum ReplicaPublicationStage {
    DiscoveryPending,
    AchievedScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaPublication {
    pub format: u32,
    pub scope: ReplicaScope,
    pub replication_address: String,
    pub generation: u64,
    pub stage: ReplicaPublicationStage,
    pub role: ReplicaRole,
}

impl ReplicaPublication {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.scope.validate()?;
        if self.format != ORDINARY_ACTION_FORMAT
            || self.generation == 0
            || self.replication_address.trim().is_empty()
            || (self.stage == ReplicaPublicationStage::DiscoveryPending
                && self.role != ReplicaRole::None)
            || (self.stage == ReplicaPublicationStage::AchievedScope
                && self.role == ReplicaRole::None)
        {
            return Err("incompatible or incomplete replica publication");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteReplicaSelection {
    pub source: ReplicaPublication,
    pub target: ReplicaPublication,
    pub target_control_address: String,
    pub generation: u64,
}

impl RemoteReplicaSelection {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.source.validate()?;
        self.target.validate()?;
        let source_configuration = &self.source.scope.configuration;
        let target_configuration = &self.target.scope.configuration;
        let replacement = source_configuration.fresh_idle_replacement(
            &self.target.scope.storage,
            &self.target.scope.process_session_id,
        ) && target_configuration.fresh_idle_replacement(
            &self.target.scope.storage,
            &self.target.scope.process_session_id,
        ) && source_configuration.current == target_configuration.current
            && source_configuration.previous.is_none()
            && target_configuration.previous.is_none()
            && source_configuration.current.primary_id
                == self.source.scope.storage.replica.replica_id
            && source_configuration.ordinary().validate().is_ok()
            && target_configuration.replicas.len() == source_configuration.replicas.len() + 1
            && source_configuration
                .replicas
                .iter()
                .all(|peer| target_configuration.replicas.contains(peer))
            && target_configuration
                .replicas
                .iter()
                .filter(|peer| {
                    peer.identity == self.source.scope.storage.replica
                        && peer.storage_incarnation == self.source.scope.storage
                        && peer.process_session_id == self.source.scope.process_session_id
                        && peer.replication_address == self.source.replication_address
                        && peer.role == ReplicaRole::Primary
                        && peer.is_up
                        && peer.is_ready
                })
                .count()
                == 1;
        if self.generation == 0
            || self.target_control_address.is_empty()
            || self.source.stage != ReplicaPublicationStage::AchievedScope
            || self.target.stage != ReplicaPublicationStage::AchievedScope
            || self.source.role != ReplicaRole::Primary
            || self.target.role != ReplicaRole::IdleSecondary
            || self.source.scope.storage.replica == self.target.scope.storage.replica
            || self.source.scope.history != self.target.scope.history
            || self.source.scope.epoch != self.target.scope.epoch
            || (self.source.scope.configuration != self.target.scope.configuration && !replacement)
            || std::iter::once(&self.source.scope.configuration.current)
                .chain(self.source.scope.configuration.previous.iter())
                .any(|configuration| {
                    configuration.members.iter().any(|member| {
                        member.identity.replica_id == self.target.scope.storage.replica.replica_id
                    }) && !replacement
                })
        {
            return Err("remote selection requires exact achieved source and prepared idle target");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", tag = "kind", deny_unknown_fields)]
#[schemars(with = "ReplicaGoalKindSchema")]
pub enum ReplicaGoalKind {
    Discover,
    RestoreRetainedMember {
        qualification: Box<RetainedMemberQualification>,
    },
    Prepare {
        role: ReplicaRole,
    },
    Reconfigure {
        role: ReplicaRole,
        #[serde(rename = "catchUp", skip_serializing_if = "Option::is_none")]
        catch_up: Option<PublicCatchUpMode>,
    },
    RefreshPrimary {
        #[serde(rename = "catchUp", skip_serializing_if = "Option::is_none")]
        catch_up: Option<PublicCatchUpMode>,
    },
    Build {
        remote: Box<RemoteReplicaSelection>,
    },
    Remove {
        remote: Box<RemoteReplicaSelection>,
    },
    Close,
    Abort,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase", deny_unknown_fields)]
#[allow(dead_code)]
struct ReplicaGoalKindSchema {
    kind: ReplicaGoalKindTag,
    role: Option<ReplicaRole>,
    catch_up: Option<PublicCatchUpMode>,
    remote: Option<Box<RemoteReplicaSelection>>,
    qualification: Option<Box<RetainedMemberQualification>>,
}

#[derive(JsonSchema)]
#[schemars(rename_all = "camelCase")]
#[allow(dead_code)]
enum ReplicaGoalKindTag {
    Discover,
    RestoreRetainedMember,
    Prepare,
    Reconfigure,
    RefreshPrimary,
    Build,
    Remove,
    Close,
    Abort,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaGoal {
    #[schemars(range(min = 19, max = 19))]
    pub format: u32,
    pub identity: PublicV1Identity,
    pub operation_id: OperationId,
    pub revision: u64,
    pub scope: ReplicaScope,
    pub desired: ReplicaGoalKind,
}

impl ReplicaGoal {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.format != ORDINARY_ACTION_FORMAT
            || !self.identity.is_public_v1()
            || self.operation_id.is_empty()
            || self.revision == 0
        {
            return Err("incompatible or incomplete replica goal");
        }
        self.scope.validate()?;
        match &self.desired {
            ReplicaGoalKind::RestoreRetainedMember { qualification } => {
                // Dormant until the parent advances the complete matched format.
                if self.format != 19
                    || qualification.predecessor_process_session_id.is_empty()
                    || qualification.predecessor_creation_id.is_empty()
                    || qualification.admitted_operation_id.is_empty()
                    || qualification.admitted_revision == 0
                    || qualification.admitted_revision >= self.revision
                    || qualification.predecessor_process_session_id == self.scope.process_session_id
                    || qualification.predecessor_creation_id == self.scope.creation_id
                    || qualification.configuration != self.scope.configuration.current
                    || self.scope.configuration.previous.is_some()
                    || self.scope.configuration.replicas.iter().any(|peer| {
                        peer.current_progress != INVALID_LSN
                            || peer.catch_up_capability != INVALID_LSN
                            || peer.must_catchup
                    })
                {
                    return Err(
                        "retained restoration requires exact historical current-only qualification without progress",
                    );
                }
                let mut validation = self.clone();
                validation.desired = ReplicaGoalKind::Prepare {
                    role: ReplicaRole::ActiveSecondary,
                };
                validation.validate()?;
            }
            ReplicaGoalKind::Discover => {
                if !self.scope.configuration.replicas.is_empty() {
                    return Err("discovery goal carries premature engine peer descriptions");
                }
            }
            ReplicaGoalKind::RefreshPrimary { catch_up } => {
                if matches!(catch_up, Some(PublicCatchUpMode::All))
                    || (catch_up.is_none() && self.scope.configuration.previous.is_some())
                {
                    return Err(
                        "Primary refresh requires preparation or exact WriteQuorum replacement",
                    );
                }
                let mut validation = self.clone();
                validation.desired = ReplicaGoalKind::Reconfigure {
                    role: ReplicaRole::Primary,
                    catch_up: None,
                };
                validation.validate()?;
            }
            ReplicaGoalKind::Prepare { role, .. } | ReplicaGoalKind::Reconfigure { role, .. } => {
                if *role != ReplicaRole::IdleSecondary {
                    self.scope.configuration.ordinary().validate()?;
                    let local = &self.scope.storage.replica;
                    let members = std::iter::once(&self.scope.configuration.current)
                        .chain(self.scope.configuration.previous.iter())
                        .flat_map(|set| set.members.iter())
                        .map(|member| &member.identity)
                        .filter(|identity| *identity != local)
                        .collect::<std::collections::BTreeSet<_>>();
                    let described = self
                        .scope
                        .configuration
                        .replicas
                        .iter()
                        .map(|peer| &peer.identity)
                        .collect::<std::collections::BTreeSet<_>>();
                    if described.contains(local) || members != described {
                        return Err(
                            "replica goal must describe exactly the nonlocal configuration members",
                        );
                    }
                }
                if !matches!(
                    role,
                    ReplicaRole::Primary
                        | ReplicaRole::IdleSecondary
                        | ReplicaRole::ActiveSecondary
                ) {
                    return Err("invalid prepared replica role");
                }
                if *role == ReplicaRole::IdleSecondary
                    && std::iter::once(&self.scope.configuration.current)
                        .chain(self.scope.configuration.previous.iter())
                        .any(|configuration| {
                            configuration.members.iter().any(|member| {
                                member.identity.replica_id == self.scope.storage.replica.replica_id
                            })
                        })
                    && !self
                        .scope
                        .configuration
                        .fresh_idle_replacement(&self.scope.storage, &self.scope.process_session_id)
                {
                    return Err("idle preparation cannot overlap selected voting membership");
                }
                if *role != ReplicaRole::IdleSecondary
                    && !self
                        .scope
                        .configuration
                        .current
                        .members
                        .iter()
                        .any(|member| {
                            member.identity == self.scope.storage.replica && member.role == *role
                        })
                {
                    return Err("goal role differs from selected configuration");
                }
            }
            ReplicaGoalKind::Build { remote } => {
                remote.validate()?;
                if remote.source.scope != self.scope {
                    return Err("remote source differs from goal scope");
                }
            }
            ReplicaGoalKind::Remove { remote } => {
                remote.validate()?;
                if !remote.source.scope.compatible_with(&self.scope) {
                    return Err("remote removal changed its exact source creation");
                }
            }
            ReplicaGoalKind::Close | ReplicaGoalKind::Abort => {}
        }
        Ok(())
    }

    pub fn intent(&self) -> PublicOperationIntent {
        let program = PublicOperationProgram::AgentGoal(Box::new(self.clone()));
        PublicOperationIntent {
            identity: self.identity.clone(),
            operation_id: self.operation_id.clone(),
            revision: self.revision,
            process_session_id: self.scope.process_session_id.clone(),
            class: self.class(),
            input_digest: program.digest(),
            replay_disposition: self.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        }
    }

    pub(crate) fn class(&self) -> PublicOperationClass {
        match &self.desired {
            ReplicaGoalKind::Build { remote } => PublicOperationClass::Build {
                target: remote.target.scope.storage.replica.replica_id,
            },
            ReplicaGoalKind::Remove { remote } => PublicOperationClass::Remove {
                target: remote.target.scope.storage.replica.replica_id,
            },
            ReplicaGoalKind::Close => PublicOperationClass::Close,
            ReplicaGoalKind::Abort => PublicOperationClass::Abort,
            _ => PublicOperationClass::Authority,
        }
    }

    pub(crate) const fn replay_disposition(&self) -> PublicOperationReplayDisposition {
        match &self.desired {
            ReplicaGoalKind::Build { .. } => {
                PublicOperationReplayDisposition::RetainBuildSuccessRetireAmbiguity
            }
            ReplicaGoalKind::Remove { .. } => PublicOperationReplayDisposition::ExactRemoval,
            ReplicaGoalKind::Close | ReplicaGoalKind::Abort => {
                PublicOperationReplayDisposition::Containment
            }
            _ => PublicOperationReplayDisposition::Convergent,
        }
    }

    #[cfg(feature = "host")]
    pub(crate) fn role(&self) -> Option<ReplicaRole> {
        match self.desired {
            ReplicaGoalKind::RestoreRetainedMember { .. } => Some(ReplicaRole::ActiveSecondary),
            ReplicaGoalKind::RefreshPrimary { .. } => Some(ReplicaRole::Primary),
            ReplicaGoalKind::Prepare { role, .. } | ReplicaGoalKind::Reconfigure { role, .. } => {
                Some(role)
            }
            _ => None,
        }
    }

    #[cfg(feature = "host")]
    pub(crate) fn remote(&self) -> Option<&RemoteReplicaSelection> {
        match &self.desired {
            ReplicaGoalKind::Build { remote } | ReplicaGoalKind::Remove { remote } => Some(remote),
            _ => None,
        }
    }
}

impl PublicOperationIntent {
    #[cfg(feature = "host")]
    pub(crate) fn goal(&self) -> Option<&ReplicaGoal> {
        match self.program.as_ref()? {
            PublicOperationProgram::AgentGoal(goal) => Some(goal),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", content = "message", deny_unknown_fields)]
#[schemars(with = "ReplicaGoalStatusSchema")]
pub enum ReplicaGoalStatus {
    Pending,
    Achieved,
    Failed(String),
    Uncertain(String),
}

#[derive(JsonSchema)]
#[schemars(deny_unknown_fields)]
#[allow(dead_code)]
struct ReplicaGoalStatusSchema {
    state: ReplicaGoalStatusTag,
    message: Option<String>,
}

#[derive(JsonSchema)]
#[allow(dead_code)]
enum ReplicaGoalStatusTag {
    Pending,
    Achieved,
    Failed,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaGoalObservation {
    pub goal: ReplicaGoal,
    pub status: ReplicaGoalStatus,
    pub selected_call: Option<String>,
    pub receipts: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaAgentSnapshot {
    pub format: u32,
    pub identity: PublicV1Identity,
    pub history: HistoryContext,
    pub storage: ReplicaStorageIncarnationContext,
    pub open_mode: OpenMode,
    pub read_status: AccessStatus,
    pub write_status: AccessStatus,
    pub withdrawn: bool,
    pub provider_writer_held: bool,
    pub process_session_id: ProcessSessionId,
    pub creation_id: ReplicatorCreationId,
    pub publication: Option<ReplicaPublication>,
    pub goals: Vec<ReplicaGoalObservation>,
    pub remote_operations: Vec<RemoteReplicaObservation>,
    pub progress: ProgressQueryResult,
    pub capability: ProgressQueryResult,
    /// Includes fault-only operations which are not projected as ordinary goals.
    #[serde(default)]
    pub accepted_revision_high_watermark: u64,
    #[serde(default)]
    pub retained_recovery_exit: Option<RetainedRecoveryExitNotice>,
    /// Agent-checked historical eligibility, never a new native-role receipt.
    #[serde(default)]
    pub retained_member_qualification: Option<RetainedMemberQualification>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "state", content = "snapshot", deny_unknown_fields)]
pub enum ReplicaAgentQueryResult {
    Uninitialized(crate::protocol::observation::UninitializedAgentObservation),
    Initialized(Box<ReplicaAgentSnapshot>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum RemoteReplicaState {
    Absent,
    InBuild,
    Ready,
    InDrop,
    Dropped,
    Uncertain,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RemoteReplicaObservation {
    pub operation_id: OperationId,
    pub selection: RemoteReplicaSelection,
    pub state: RemoteReplicaState,
    pub accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReplicaDiscoveryQuery {
    pub format: u32,
    pub history: HistoryContext,
    pub replica: ReplicaIdentity,
    pub process_session_id: Option<ProcessSessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReplicaGoalAcknowledgement {
    pub goal: ReplicaGoal,
    pub logically_retired: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn m2_ordinary_reconfigure_without_catch_up_omits_null_enum() {
        for catch_up in [
            None,
            Some(PublicCatchUpMode::WriteQuorum),
            Some(PublicCatchUpMode::All),
        ] {
            let goal = ReplicaGoalKind::Reconfigure {
                role: ReplicaRole::Primary,
                catch_up,
            };
            let json = serde_json::to_value(&goal).unwrap();
            if catch_up.is_none() {
                assert!(json.get("catchUp").is_none());
            } else {
                assert_eq!(json["catchUp"], serde_json::to_value(catch_up).unwrap());
            }
            assert_eq!(
                serde_json::from_value::<ReplicaGoalKind>(json).unwrap(),
                goal
            );
        }
    }
}
