use async_trait::async_trait;
use bytes::Bytes;
pub use kuberic_protocol::types::{BuildAuthority, BuildAuthorityKind};
use kuberic_protocol::types::{
    ConfigurationDescriptor, ConfigurationId, EffectivePolicy, Epoch, OperationId, ReplicaIdentity,
    ReplicaRole, SwitchoverHandoff, TransitionKind,
};
use kuberic_protocol::types::{
    ReplicaRetirementReport, SecondaryRemovalEvidence, SecondaryRemovalPreparation,
    SecondaryScaleDownCleanup,
};
use kuberic_protocol::validation::{validate_configuration, validate_transition_relationship};
use kuberic_protocol::validation::{
    validate_replica_retirement, validate_secondary_removal_evidence,
    validate_secondary_removal_preparation, validate_secondary_scale_down_cleanup,
};
use serde::{Deserialize, Serialize};

use crate::transport::{CopyItem, ReplicationAck, ReplicationItem};
use crate::{ContractError, Result};

pub fn validate_build_envelope(authority: &BuildAuthority, envelope: &CopyItem) -> Result<()> {
    authority
        .validate()
        .map_err(|error| ContractError::AuthorityMismatch(error.to_string()))?;
    if envelope.build_id != authority.build_id
        || envelope.sender != authority.source
        || envelope.receiver != authority.target
        || envelope.epoch != authority.current_configuration.epoch
        || envelope.current_configuration_id != authority.current_configuration.configuration_id
        || envelope.replication_boundary_lsn != authority.replication_boundary_lsn
    {
        return Err(ContractError::AuthorityMismatch(
            "copy item does not match durable build authority".to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AuthorityFence {
    pub epoch: Epoch,
    pub previous_configuration_id: Option<ConfigurationId>,
    pub current_configuration_id: ConfigurationId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicationProgress {
    pub fence: AuthorityFence,
    pub verified_lsn: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableBuildProgress {
    pub authority: BuildAuthority,
    pub last_sequence: u64,
    pub durable_lsn: i64,
    pub completed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LocalWritePhase {
    Reserved,
    Registered,
    Committed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DurableLocalWrite {
    pub operation_id: OperationId,
    pub lsn: i64,
    #[serde(default)]
    pub committed_lsn: i64,
    pub data: Bytes,
    pub phase: LocalWritePhase,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmittedAuthority {
    pub local_identity: ReplicaIdentity,
    pub transition_kind: Option<TransitionKind>,
    pub previous_configuration: Option<ConfigurationDescriptor>,
    pub current_configuration: ConfigurationDescriptor,
    #[serde(default)]
    pub switchover_handoff: Option<SwitchoverHandoff>,
    #[serde(default)]
    pub secondary_removal: Option<SecondaryRemovalEvidence>,
}

impl AdmittedAuthority {
    pub fn is_current_only_completion_of(&self, existing: &Self) -> bool {
        self.local_identity == existing.local_identity
            && self.current_configuration == existing.current_configuration
            && existing.previous_configuration.is_some()
            && self.previous_configuration.is_none()
            && self.transition_kind.is_none()
            && self.switchover_handoff == existing.switchover_handoff
            && match (&self.secondary_removal, &existing.secondary_removal) {
                (Some(next), Some(old)) => {
                    next.preparation == old.preparation
                        && next.previous_read_quorum == old.previous_read_quorum
                        && (old.reduced_write_quorum.is_empty()
                            || next.reduced_write_quorum == old.reduced_write_quorum)
                        && validate_secondary_removal_evidence(next, true).is_ok()
                }
                (None, None) => true,
                _ => false,
            }
    }

    pub fn fence(&self) -> AuthorityFence {
        AuthorityFence {
            epoch: self.current_configuration.epoch,
            previous_configuration_id: self
                .previous_configuration
                .as_ref()
                .map(|configuration| configuration.configuration_id.clone()),
            current_configuration_id: self.current_configuration.configuration_id.clone(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        if let Some(evidence) = &self.secondary_removal {
            validate_secondary_removal_evidence(evidence, self.previous_configuration.is_none())
                .map_err(|error| ContractError::AuthorityMismatch(error.to_string()))?;
            let intent = &evidence.preparation.intent;
            if self.current_configuration != intent.current_configuration
                || self.previous_configuration.as_ref()
                    != self
                        .previous_configuration
                        .as_ref()
                        .map(|_| &intent.previous_configuration)
                || self.transition_kind
                    != self
                        .previous_configuration
                        .as_ref()
                        .map(|_| TransitionKind::SecondaryScaleDown)
                || self.switchover_handoff.is_some()
                || !intent
                    .current_configuration
                    .members
                    .iter()
                    .any(|m| m.identity == self.local_identity)
            {
                return Err(ContractError::AuthorityMismatch(
                    "reduction differs from exact prepared authority".into(),
                ));
            }
            return Ok(());
        }
        validate_configuration(&self.current_configuration, None)
            .map_err(|error| ContractError::AuthorityMismatch(error.to_string()))?;
        let policy = EffectivePolicy::fixed(self.current_configuration.members.len() as u32, 0)
            .ok_or_else(|| {
                ContractError::AuthorityMismatch("configuration must contain members".to_string())
            })?;
        match (self.previous_configuration.as_ref(), self.transition_kind) {
            (
                Some(previous),
                Some(
                    kind @ (TransitionKind::Replacement
                    | TransitionKind::Failover
                    | TransitionKind::PlannedSwitchover),
                ),
            ) => {
                validate_transition_relationship(
                    kind,
                    Some(previous),
                    &self.current_configuration,
                    &policy,
                )
                .map_err(|error| ContractError::AuthorityMismatch(error.to_string()))?;
            }
            (None, Some(TransitionKind::Bootstrap)) | (None, None) => {}
            (Some(_), _) => {
                return Err(ContractError::AuthorityMismatch(
                    "Previous Configuration requires replacement, failover, or planned switchover authority"
                        .to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(ContractError::AuthorityMismatch(
                    "non-bootstrap transition requires a Previous Configuration".to_string(),
                ));
            }
        }
        if let Some(handoff) = &self.switchover_handoff {
            let previous_relationship_valid =
                self.previous_configuration.as_ref().is_none_or(|previous| {
                    let source_is_primary = previous.members.iter().any(|member| {
                        member.identity == handoff.source && member.role == ReplicaRole::Primary
                    });
                    let target_is_primary = previous.members.iter().any(|member| {
                        member.identity == handoff.target && member.role == ReplicaRole::Primary
                    });
                    if source_is_primary {
                        previous.configuration_id == handoff.starting_configuration_id
                            && previous.epoch == handoff.starting_epoch
                            && self.current_configuration.primary_id == handoff.target.replica_id
                    } else {
                        target_is_primary
                            && self.current_configuration.primary_id == handoff.source.replica_id
                            && previous.epoch.data_loss_number
                                == handoff.starting_epoch.data_loss_number
                            && previous.epoch.configuration_number
                                > handoff.starting_epoch.configuration_number
                    }
                });
            if (self.transition_kind != Some(TransitionKind::PlannedSwitchover)
                && self.previous_configuration.is_some())
                || handoff.preparation_generation == 0
                || !previous_relationship_valid
                || handoff.starting_epoch.data_loss_number
                    != self.current_configuration.epoch.data_loss_number
                || handoff.starting_epoch.configuration_number
                    >= self.current_configuration.epoch.configuration_number
                || handoff.source == handoff.target
                || !self
                    .current_configuration
                    .members
                    .iter()
                    .any(|member| member.identity == handoff.source)
                || !self
                    .current_configuration
                    .members
                    .iter()
                    .any(|member| member.identity == handoff.target)
                || (self.current_configuration.primary_id != handoff.source.replica_id
                    && self.current_configuration.primary_id != handoff.target.replica_id)
            {
                return Err(ContractError::AuthorityMismatch(
                    "switchover handoff contradicts admitted authority".to_string(),
                ));
            }
        } else if self.transition_kind == Some(TransitionKind::PlannedSwitchover) {
            return Err(ContractError::AuthorityMismatch(
                "planned switchover authority requires its handoff certificate".to_string(),
            ));
        }
        if !self.contains_member(&self.local_identity) {
            return Err(ContractError::AuthorityMismatch(
                "local identity is outside admitted authority".to_string(),
            ));
        }
        Ok(())
    }

    pub fn primary_identity(&self) -> &ReplicaIdentity {
        &self
            .current_configuration
            .members
            .iter()
            .find(|member| {
                member.identity.replica_id == self.current_configuration.primary_id
                    && member.role == ReplicaRole::Primary
            })
            .expect("validated configuration has one primary")
            .identity
    }

    pub fn contains_member(&self, identity: &ReplicaIdentity) -> bool {
        self.current_configuration
            .members
            .iter()
            .any(|member| &member.identity == identity)
            || self
                .previous_configuration
                .as_ref()
                .is_some_and(|previous| {
                    previous
                        .members
                        .iter()
                        .any(|member| &member.identity == identity)
                })
    }

    pub fn local_role(&self) -> ReplicaRole {
        self.current_configuration
            .members
            .iter()
            .find(|member| member.identity == self.local_identity)
            .or_else(|| {
                self.previous_configuration.as_ref().and_then(|previous| {
                    previous
                        .members
                        .iter()
                        .find(|member| member.identity == self.local_identity)
                })
            })
            .expect("validated local identity belongs to authority")
            .role
    }

    pub fn validate_envelope(&self, envelope: &ReplicationItem) -> Result<()> {
        self.validate_fence(
            &envelope.sender,
            &envelope.receiver,
            envelope.epoch,
            envelope.previous_configuration_id.as_ref(),
            &envelope.current_configuration_id,
        )
    }

    pub fn validate_acknowledgement(&self, acknowledgement: &ReplicationAck) -> Result<()> {
        self.validate_fence(
            &acknowledgement.sender,
            &acknowledgement.receiver,
            acknowledgement.epoch,
            acknowledgement.previous_configuration_id.as_ref(),
            &acknowledgement.current_configuration_id,
        )
    }

    fn validate_fence(
        &self,
        sender: &ReplicaIdentity,
        receiver: &ReplicaIdentity,
        epoch: kuberic_protocol::types::Epoch,
        previous_configuration_id: Option<&kuberic_protocol::types::ConfigurationId>,
        current_configuration_id: &kuberic_protocol::types::ConfigurationId,
    ) -> Result<()> {
        if sender != self.primary_identity() {
            return Err(ContractError::AuthorityMismatch(
                "replication sender is not the exact admitted primary".to_string(),
            ));
        }
        if !self.contains_member(receiver) {
            return Err(ContractError::AuthorityMismatch(
                "replication receiver is outside admitted authority".to_string(),
            ));
        }
        if epoch != self.current_configuration.epoch
            || current_configuration_id != &self.current_configuration.configuration_id
            || previous_configuration_id
                != self
                    .previous_configuration
                    .as_ref()
                    .map(|configuration| &configuration.configuration_id)
        {
            return Err(ContractError::AuthorityMismatch(
                "replication epoch or configuration fence is stale".to_string(),
            ));
        }
        Ok(())
    }
}

/// Durable runtime outputs. The agent must persist these before acknowledging
/// the enclosing effect and load the tombstone before opening application state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetiredAuthority {
    pub committed: SecondaryScaleDownCleanup,
    pub report: ReplicaRetirementReport,
}

impl RetiredAuthority {
    pub fn validate(&self, local: &ReplicaIdentity) -> Result<()> {
        validate_secondary_scale_down_cleanup(&self.committed)
            .and_then(|_| validate_replica_retirement(&self.report))
            .map_err(|error| ContractError::AuthorityMismatch(error.to_string()))?;
        if self.report.intent != self.committed.evidence.preparation.intent
            || &self.report.intent.target != local
            || self
                .committed
                .retirement
                .as_ref()
                .is_some_and(|r| r != &self.report)
        {
            return Err(ContractError::AuthorityMismatch(
                "retirement target or evidence differs".into(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
pub trait ReplicaAuthorityStore: Send + Sync {
    async fn load(&self) -> Result<Option<AdmittedAuthority>>;

    async fn admit(&self, authority: &AdmittedAuthority) -> Result<()>;

    async fn load_secondary_removal(&self) -> Result<Option<SecondaryRemovalPreparation>> {
        Ok(None)
    }

    /// Replay is exact. A later preparation may replace a completed predecessor
    /// only with a starting epoch at least as new as its reduced configuration.
    async fn record_secondary_removal(
        &self,
        preparation: &SecondaryRemovalPreparation,
    ) -> Result<()> {
        validate_secondary_removal_preparation(preparation)
            .map_err(|error| ContractError::AuthorityMismatch(error.to_string()))?;
        Err(ContractError::Persistence(
            "secondary-removal persistence is unavailable".into(),
        ))
    }

    async fn load_retired_authority(&self) -> Result<Option<RetiredAuthority>> {
        Ok(None)
    }

    async fn load_secondary_removal_commit(&self) -> Result<Option<SecondaryScaleDownCleanup>> {
        Ok(None)
    }

    async fn record_secondary_removal_commit(
        &self,
        _committed: &SecondaryScaleDownCleanup,
    ) -> Result<()> {
        Err(ContractError::Persistence(
            "secondary-removal commit persistence is unavailable".into(),
        ))
    }

    /// Must atomically reject conflicting tombstones and all subsequent active admission.
    async fn retire(&self, _authority: &RetiredAuthority) -> Result<()> {
        Err(ContractError::Persistence(
            "retirement persistence is unavailable".into(),
        ))
    }
}

#[async_trait]
pub trait ReplicationProgressStore: Send + Sync {
    async fn load_replication_progress(
        &self,
        fence: &AuthorityFence,
    ) -> Result<Option<ReplicationProgress>>;

    async fn load_configuration_progress(
        &self,
        epoch: Epoch,
        current_configuration_id: &ConfigurationId,
    ) -> Result<Option<ReplicationProgress>>;

    async fn record_replication_progress(&self, progress: &ReplicationProgress) -> Result<()>;
}

#[async_trait]
pub trait LocalWriteJournal: Send + Sync {
    async fn load_local_write(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<DurableLocalWrite>>;

    async fn load_local_writes(&self) -> Result<Vec<DurableLocalWrite>>;

    async fn record_local_write(&self, write: &DurableLocalWrite) -> Result<()>;

    async fn reset_local_writes_after_data_loss(&self, committed_lsn: i64) -> Result<()>;
}

#[async_trait]
pub trait BuildAuthorityStore: Send + Sync {
    async fn load_build(&self, build_id: &OperationId) -> Result<Option<BuildAuthority>>;

    async fn admit_build(&self, authority: &BuildAuthority) -> Result<()>;
}

#[async_trait]
pub trait BuildProgressStore: Send + Sync {
    async fn load_build_progress(
        &self,
        build_id: &OperationId,
    ) -> Result<Option<DurableBuildProgress>>;

    async fn record_build_progress(&self, progress: &DurableBuildProgress) -> Result<()>;
}

pub trait AuthorityStore:
    ReplicaAuthorityStore
    + ReplicationProgressStore
    + LocalWriteJournal
    + BuildAuthorityStore
    + BuildProgressStore
{
}

impl<T> AuthorityStore for T where
    T: ReplicaAuthorityStore
        + ReplicationProgressStore
        + LocalWriteJournal
        + BuildAuthorityStore
        + BuildProgressStore
{
}

#[cfg(test)]
#[allow(dead_code)]
#[path = "../../kuberic-protocol/tests/support/secondary_scale_down.rs"]
mod removal_fixture;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduction_requires_independent_policies_and_frozen_old_read_evidence() {
        for size in 2..=5 {
            let intent = removal_fixture::intent(&(1..=size).collect::<Vec<_>>(), 1);
            let mut evidence = removal_fixture::evidence(&intent);
            evidence.reduced_write_quorum.clear();
            let mut authority = AdmittedAuthority {
                local_identity: intent.primary.clone(),
                transition_kind: Some(TransitionKind::SecondaryScaleDown),
                previous_configuration: Some(intent.previous_configuration.clone()),
                current_configuration: intent.current_configuration.clone(),
                switchover_handoff: None,
                secondary_removal: Some(evidence),
            };
            authority.validate().unwrap();
            let good = authority.clone();
            authority
                .secondary_removal
                .as_mut()
                .unwrap()
                .previous_read_quorum
                .truncate(intent.previous_policy.read_quorum as usize - 1);
            assert!(authority.validate().is_err());
            authority = good.clone();
            authority
                .secondary_removal
                .as_mut()
                .unwrap()
                .preparation
                .intent
                .current_policy = intent.previous_policy.clone();
            assert!(authority.validate().is_err());
            let mut completed = good.clone();
            completed.previous_configuration = None;
            completed.transition_kind = None;
            assert!(
                completed.validate().is_err(),
                "current-only needs the frozen reduced write quorum"
            );
            completed.secondary_removal = Some(removal_fixture::evidence(&intent));
            completed.validate().unwrap();
            assert!(completed.is_current_only_completion_of(&good));
            completed
                .secondary_removal
                .as_mut()
                .unwrap()
                .previous_read_quorum[0]
                .report_sequence += 1;
            assert!(!completed.is_current_only_completion_of(&good));
        }
    }

    #[test]
    fn retirement_binds_terminal_postconditions_and_exact_committed_target() {
        let intent = removal_fixture::intent(&[1, 2, 3], 1);
        let retired = RetiredAuthority {
            committed: removal_fixture::cleanup(&intent),
            report: removal_fixture::retirement(&intent),
        };
        retired.validate(&intent.target).unwrap();
        assert!(retired.validate(&intent.primary).is_err());
        for mutate in [
            |r: &mut RetiredAuthority| r.report.application_closed = false,
            |r: &mut RetiredAuthority| r.report.peers_fenced = false,
            |r: &mut RetiredAuthority| r.report.role = ReplicaRole::ActiveSecondary,
            |r: &mut RetiredAuthority| {
                r.report.write_status = kuberic_protocol::types::AccessStatus::Granted
            },
            |r: &mut RetiredAuthority| r.committed.current_only_write_quorum.clear(),
        ] {
            let mut invalid = retired.clone();
            mutate(&mut invalid);
            assert!(invalid.validate(&intent.target).is_err());
        }
    }
}
