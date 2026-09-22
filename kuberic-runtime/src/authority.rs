use async_trait::async_trait;
use kuberic_protocol::types::{
    ConfigurationDescriptor, EffectivePolicy, OperationId, ReplicaIdentity, ReplicaRole,
    TransitionKind,
};
use kuberic_protocol::validation::{validate_configuration, validate_transition_relationship};
use kuberic_wire::{ReplicationAcknowledgement, ReplicationEnvelope};

use crate::{Result, RuntimeError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildAuthority {
    pub build_id: OperationId,
    pub source: ReplicaIdentity,
    pub target: ReplicaIdentity,
    pub current_configuration: ConfigurationDescriptor,
    pub replication_boundary_lsn: i64,
}

impl BuildAuthority {
    pub fn validate(&self) -> Result<()> {
        validate_configuration(&self.current_configuration, None)
            .map_err(|error| RuntimeError::AuthorityMismatch(error.to_string()))?;
        let primary = self
            .current_configuration
            .members
            .iter()
            .find(|member| {
                member.identity.replica_id == self.current_configuration.primary_id
                    && member.role == ReplicaRole::Primary
            })
            .expect("validated configuration has one primary");
        if primary.identity != self.source {
            return Err(RuntimeError::AuthorityMismatch(
                "copy source is not the exact Current Configuration primary".to_string(),
            ));
        }
        if self
            .current_configuration
            .members
            .iter()
            .any(|member| member.identity == self.target)
        {
            return Err(RuntimeError::AuthorityMismatch(
                "copy target must remain outside configuration membership".to_string(),
            ));
        }
        if self.replication_boundary_lsn < 0 {
            return Err(RuntimeError::AuthorityMismatch(
                "copy boundary must not be negative".to_string(),
            ));
        }
        Ok(())
    }

    pub fn validate_envelope(&self, envelope: &kuberic_wire::CopyEnvelope) -> Result<()> {
        if envelope.build_id != self.build_id
            || envelope.sender != self.source
            || envelope.receiver != self.target
            || envelope.epoch != self.current_configuration.epoch
            || envelope.current_configuration_id != self.current_configuration.configuration_id
            || envelope.replication_boundary_lsn != self.replication_boundary_lsn
        {
            return Err(RuntimeError::AuthorityMismatch(
                "copy item does not match durable build authority".to_string(),
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdmittedAuthority {
    pub local_identity: ReplicaIdentity,
    pub transition_kind: Option<TransitionKind>,
    pub previous_configuration: Option<ConfigurationDescriptor>,
    pub current_configuration: ConfigurationDescriptor,
}

impl AdmittedAuthority {
    pub fn validate(&self) -> Result<()> {
        validate_configuration(&self.current_configuration, None)
            .map_err(|error| RuntimeError::AuthorityMismatch(error.to_string()))?;
        let policy = EffectivePolicy::fixed(self.current_configuration.members.len() as u32, 0)
            .ok_or_else(|| {
                RuntimeError::AuthorityMismatch("configuration must contain members".to_string())
            })?;
        match (self.previous_configuration.as_ref(), self.transition_kind) {
            (
                Some(previous),
                Some(kind @ (TransitionKind::Replacement | TransitionKind::Failover)),
            ) => {
                validate_transition_relationship(
                    kind,
                    Some(previous),
                    &self.current_configuration,
                    &policy,
                )
                .map_err(|error| RuntimeError::AuthorityMismatch(error.to_string()))?;
            }
            (None, Some(TransitionKind::Bootstrap)) | (None, None) => {}
            (Some(_), _) => {
                return Err(RuntimeError::AuthorityMismatch(
                    "Previous Configuration requires replacement or failover authority".to_string(),
                ));
            }
            (None, Some(_)) => {
                return Err(RuntimeError::AuthorityMismatch(
                    "non-bootstrap transition requires a Previous Configuration".to_string(),
                ));
            }
        }
        if !self.contains_member(&self.local_identity) {
            return Err(RuntimeError::AuthorityMismatch(
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

    pub fn validate_envelope(&self, envelope: &ReplicationEnvelope) -> Result<()> {
        self.validate_fence(
            &envelope.sender,
            &envelope.receiver,
            envelope.epoch,
            envelope.previous_configuration_id.as_ref(),
            &envelope.current_configuration_id,
        )
    }

    pub fn validate_acknowledgement(
        &self,
        acknowledgement: &ReplicationAcknowledgement,
    ) -> Result<()> {
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
            return Err(RuntimeError::AuthorityMismatch(
                "replication sender is not the exact admitted primary".to_string(),
            ));
        }
        if !self.contains_member(receiver) {
            return Err(RuntimeError::AuthorityMismatch(
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
            return Err(RuntimeError::AuthorityMismatch(
                "replication epoch or configuration fence is stale".to_string(),
            ));
        }
        Ok(())
    }
}

#[async_trait]
pub trait AuthorityStore: Send + Sync {
    async fn load(&self) -> Result<Option<AdmittedAuthority>>;

    async fn admit(&self, authority: &AdmittedAuthority) -> Result<()>;

    async fn load_build(&self, build_id: &OperationId) -> Result<Option<BuildAuthority>>;

    async fn admit_build(&self, authority: &BuildAuthority) -> Result<()>;
}
