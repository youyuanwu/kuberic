//! Fenced protocol commands and declarative Kubernetes changes.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::protocol::public_operations::{
    PublicInboundBuildAuthority, PublicOperationIntent, PublicSessionEpochBarrierStage,
    PublicSessionRenewalStage, PublicV1AcceptedStatus, PublicV1Identity,
    PublicV1SessionRenewalStatus,
};
use crate::protocol::types::{
    AgentGeneration, BuildAuthority, ConfigurationDescriptor, EffectivePolicy, Epoch,
    InitializationId, OperationId, PodUid, ProvisioningIntent, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ResourceUid, SwitchoverHandoff, SwitchoverPreparationId,
    SwitchoverRequestId,
};
use crate::protocol::types::{
    HistoryContext, HistoryInitializationId, ReplicaStorageIncarnationContext,
    derive_bootstrap_history,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EffectClass {
    ConvergentEnsure,
    ReconfigurationAction,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeAgentStore {
    pub initialization_id: InitializationId,
    pub resource_uid: ResourceUid,
    pub local_replica_id: ReplicaId,
    pub expected_instance_id: ReplicaInstanceId,
    pub expected_pod_uid: PodUid,
    pub expected_pvc_uid: PvcUid,
    pub assigned_agent_generation: AgentGeneration,
    pub effective_policy: EffectivePolicy,
    pub bootstrap_configuration: ConfigurationDescriptor,
    pub provisioning: Option<ProvisioningIntent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnsureConfiguration {
    pub operation_id: OperationId,
    pub previous_configuration: Option<ConfigurationDescriptor>,
    pub current_configuration: ConfigurationDescriptor,
    pub previous_epoch: Option<Epoch>,
    pub current_epoch: Epoch,
    pub effective_policy: EffectivePolicy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_policy: Option<EffectivePolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secondary_removal_evidence: Option<crate::protocol::types::SecondaryRemovalEvidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale_up_evidence: Option<Box<crate::protocol::types::ScaleUpConfigurationEvidence>>,
    pub local_replica_id: ReplicaId,
    pub expected_instance_id: ReplicaInstanceId,
    pub expected_agent_generation: AgentGeneration,
    pub transition_kind: crate::protocol::types::TransitionKind,
    pub failover_safe_lsn: Option<i64>,
    pub primary_write_status: crate::protocol::types::AccessStatus,
    pub current_only: bool,
    #[serde(default)]
    pub retire_build_ids: Vec<OperationId>,
    #[serde(default)]
    pub switchover_handoff: Option<SwitchoverHandoff>,
    #[serde(default)]
    pub retire_switchover_preparation_ids: Vec<SwitchoverPreparationId>,
}

impl EnsureConfiguration {
    /// Exact, write-closed retirement without admitting a newer authority.
    pub fn is_switchover_restoration(&self) -> bool {
        self.transition_kind == crate::protocol::types::TransitionKind::PlannedSwitchover
            && !self.current_only
            && self.previous_configuration.is_none()
            && self.previous_epoch.is_none()
            && self.primary_write_status
                == crate::protocol::types::AccessStatus::ReconfigurationPending
            && self.failover_safe_lsn.is_none()
            && self.retire_build_ids.is_empty()
            && self.current_epoch == self.current_configuration.epoch
            && self.current_configuration.primary_id == self.local_replica_id
            && self.retire_switchover_preparation_ids.len() == 1
            && !self.retire_switchover_preparation_ids[0]
                .operation_id
                .is_empty()
            && self.retire_switchover_preparation_ids[0].generation > 0
            && self.switchover_handoff.as_ref().is_none_or(|handoff| {
                self.current_epoch == handoff.starting_epoch
                    && self.current_configuration.epoch == handoff.starting_epoch
                    && self.current_configuration.configuration_id
                        == handoff.starting_configuration_id
                    && self.current_configuration.primary_id == handoff.source.replica_id
                    && self.local_replica_id == handoff.source.replica_id
                    && self.expected_instance_id == handoff.source.instance_id
                    && self.expected_agent_generation == handoff.source.agent_generation
                    && self.retire_switchover_preparation_ids == [handoff.preparation()]
            })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareSwitchover {
    pub preparation_generation: u64,
    pub operation_id: OperationId,
    pub request_id: SwitchoverRequestId,
    pub local_replica_id: ReplicaId,
    pub expected_instance_id: ReplicaInstanceId,
    pub expected_agent_generation: AgentGeneration,
    pub source: ReplicaIdentity,
    pub target: ReplicaIdentity,
    pub current_configuration: ConfigurationDescriptor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EnsureReplicaBuild {
    pub operation_id: OperationId,
    pub local_replica_id: ReplicaId,
    pub expected_instance_id: ReplicaInstanceId,
    pub expected_agent_generation: AgentGeneration,
    pub target: ReplicaIdentity,
    pub authority: Option<BuildAuthority>,
    pub source_session_id: Option<crate::protocol::types::ProcessSessionId>,
    #[serde(default)]
    pub retire: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PrepareSecondaryRemoval {
    pub operation_id: OperationId,
    pub local_replica_id: ReplicaId,
    pub expected_instance_id: ReplicaInstanceId,
    pub expected_agent_generation: AgentGeneration,
    pub intent: crate::protocol::types::SecondaryScaleDownIntent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetireReplica {
    pub operation_id: OperationId,
    pub local_replica_id: ReplicaId,
    pub expected_instance_id: ReplicaInstanceId,
    pub expected_agent_generation: AgentGeneration,
    pub committed: crate::protocol::types::SecondaryScaleDownCleanup,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Publish already-accepted current-only evidence locally before stable access.
pub struct AcceptSecondaryRemovalCommit {
    pub operation_id: OperationId,
    pub target: ReplicaIdentity,
    pub committed: crate::protocol::types::SecondaryScaleDownCleanup,
    /// Clear a retained secondary's historical local fence without quorum credit.
    #[serde(default)]
    pub local_recovery: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum PublicV1HistoryCommand {
    Initialize {
        identity: PublicV1Identity,
        accepted: PublicV1AcceptedStatus,
        storage: ReplicaStorageIncarnationContext,
    },
    Recover {
        accepted: PublicV1AcceptedStatus,
        intent: Box<PublicOperationIntent>,
    },
    InstallInboundBuildAuthority {
        authority: Box<PublicInboundBuildAuthority>,
    },
    RevokeInboundBuildAuthority {
        authority: Box<PublicInboundBuildAuthority>,
    },
    SessionRenewal {
        command: Box<PublicV1SessionRenewalCommand>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum PublicV1SessionRenewalCommand {
    CloseAccessAndIngress {
        accepted: PublicV1SessionRenewalStatus,
    },
    UpdateEpochBarrier {
        accepted: PublicV1SessionRenewalStatus,
        survivor: crate::protocol::types::ProcessSessionEndpoint,
        epoch: Epoch,
    },
    ReplayReplacementRole {
        accepted: PublicV1SessionRenewalStatus,
        process_session_id: crate::protocol::types::ProcessSessionId,
        epoch: Epoch,
        role: crate::protocol::types::ReplicaRole,
    },
    InstallFreshConfiguration {
        accepted: PublicV1SessionRenewalStatus,
        configuration: ConfigurationDescriptor,
    },
}

impl PublicV1SessionRenewalCommand {
    fn configuration_journal_endpoint<'a>(
        accepted: &'a PublicV1SessionRenewalStatus,
        configuration: &ConfigurationDescriptor,
    ) -> &'a crate::protocol::types::ProcessSessionEndpoint {
        accepted.required.replacement.as_ref().unwrap_or_else(|| {
            let primary = configuration
                .members
                .iter()
                .find(|member| member.identity.replica_id == configuration.primary_id)
                .expect("validated fresh configuration primary");
            accepted
                .required
                .surviving_sessions
                .iter()
                .find(|endpoint| endpoint.identity == primary.identity)
                .expect("validated fresh configuration primary endpoint")
        })
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        self.accepted().validate()?;
        match self {
            Self::CloseAccessAndIngress { accepted }
                if accepted.stage != PublicSessionRenewalStage::Required =>
            {
                Err("session-renewal access closure is not the accepted next barrier")
            }
            Self::UpdateEpochBarrier {
                accepted,
                survivor,
                epoch,
            } if accepted.stage != PublicSessionRenewalStage::EpochBarriersPending
                || epoch != &accepted.required.renewal_epoch
                || !accepted.barriers.iter().any(|barrier| {
                    barrier.survivor == *survivor
                        && barrier.stage == PublicSessionEpochBarrierStage::Pending
                }) =>
            {
                Err("session-renewal update_epoch command differs from a pending barrier")
            }
            Self::ReplayReplacementRole {
                accepted,
                process_session_id,
                epoch,
                role,
            } => {
                let replacement = accepted
                    .required
                    .replacement
                    .as_ref()
                    .ok_or("session-renewal role replay has no replacement")?;
                if accepted.stage != PublicSessionRenewalStage::ReplacementRolePending
                    || process_session_id != &replacement.process_session_id
                    || epoch != &accepted.required.renewal_epoch
                    || role != &accepted.required.predecessor.role
                {
                    return Err("session-renewal role replay differs from accepted authority");
                }
                Ok(())
            }
            Self::InstallFreshConfiguration {
                accepted,
                configuration,
            } if accepted.stage != PublicSessionRenewalStage::ConfigurationPending
                || configuration != &accepted.required.fresh_configuration =>
            {
                Err("session-renewal configuration differs from accepted authority")
            }
            _ => Ok(()),
        }
    }

    pub fn accepted(&self) -> &PublicV1SessionRenewalStatus {
        match self {
            Self::CloseAccessAndIngress { accepted }
            | Self::UpdateEpochBarrier { accepted, .. }
            | Self::ReplayReplacementRole { accepted, .. }
            | Self::InstallFreshConfiguration { accepted, .. } => accepted,
        }
    }

    pub fn target(&self) -> &ReplicaIdentity {
        match self {
            Self::CloseAccessAndIngress { accepted } => &accepted.required.predecessor.identity,
            Self::UpdateEpochBarrier { survivor, .. } => &survivor.identity,
            Self::ReplayReplacementRole { accepted, .. } => {
                &accepted
                    .required
                    .replacement
                    .as_ref()
                    .expect("validated session-renewal replacement")
                    .identity
            }

            Self::InstallFreshConfiguration {
                accepted,
                configuration,
            } => &Self::configuration_journal_endpoint(accepted, configuration).identity,
        }
    }

    pub fn expected_endpoint(&self) -> &crate::protocol::types::ProcessSessionEndpoint {
        match self {
            Self::CloseAccessAndIngress { accepted } => accepted
                .required
                .replacement
                .as_ref()
                .unwrap_or(&accepted.required.predecessor),
            Self::ReplayReplacementRole { accepted, .. } => accepted
                .required
                .replacement
                .as_ref()
                .expect("validated session-renewal replacement"),
            Self::UpdateEpochBarrier { survivor, .. } => survivor,
            Self::InstallFreshConfiguration {
                accepted,
                configuration,
            } => Self::configuration_journal_endpoint(accepted, configuration),
        }
    }
}

impl PublicV1HistoryCommand {
    pub fn validate(&self) -> Result<(), &'static str> {
        match self {
            Self::Initialize {
                identity,
                accepted,
                storage,
            } => {
                storage.validate()?;
                if !identity.is_public_v1()
                    || accepted.identity != *identity
                    || accepted.data_loss_recovery.is_some()
                    || accepted.history.resource_uid.is_empty()
                    || accepted.history.protocol_generation.value() != identity.generation
                    || accepted.history
                        != derive_bootstrap_history(
                            &accepted.history.resource_uid,
                            accepted.history.protocol_generation,
                            &accepted.history_initialization_id,
                        )
                {
                    return Err("public-v1 initialization command changed accepted history");
                }
            }
            Self::Recover { accepted, intent } => {
                intent.validate()?;
                let recovery = accepted
                    .data_loss_recovery
                    .as_ref()
                    .ok_or("public-v1 recovery command lacks accepted recovery authority")?;
                let input = intent
                    .lifecycle
                    .as_ref()
                    .and_then(|input| input.data_loss_recovery.as_ref())
                    .ok_or("public-v1 recovery command lacks typed lifecycle recovery")?;
                if accepted.identity != intent.identity
                    || recovery.stage
                        != crate::protocol::public_operations::PublicDataLossRecoveryStage::Invalidated
                    || recovery.logical_authorization() != input.authorization
                    || recovery.attempt != input.attempt
                {
                    return Err("public-v1 recovery command differs from accepted authority");
                }
            }
            Self::InstallInboundBuildAuthority { authority }
            | Self::RevokeInboundBuildAuthority { authority } => authority.validate()?,
            Self::SessionRenewal { command } => command.validate()?,
        }
        Ok(())
    }

    pub fn accepted(&self) -> Option<&PublicV1AcceptedStatus> {
        match self {
            Self::Initialize { accepted, .. } | Self::Recover { accepted, .. } => Some(accepted),
            Self::InstallInboundBuildAuthority { .. }
            | Self::RevokeInboundBuildAuthority { .. } => None,
            Self::SessionRenewal { .. } => None,
        }
    }

    pub fn target(&self) -> &ReplicaIdentity {
        match self {
            Self::Initialize { storage, .. } => &storage.replica,
            Self::Recover { accepted, .. } => {
                &accepted
                    .data_loss_recovery
                    .as_ref()
                    .expect("validated public-v1 recovery command")
                    .provisional_primary
                    .replica
            }
            Self::InstallInboundBuildAuthority { authority }
            | Self::RevokeInboundBuildAuthority { authority } => &authority.attempt.target,
            Self::SessionRenewal { command } => command.target(),
        }
    }

    pub fn resource_uid(&self) -> &ResourceUid {
        match self {
            Self::Initialize { accepted, .. } | Self::Recover { accepted, .. } => {
                &accepted.history.resource_uid
            }
            Self::InstallInboundBuildAuthority { authority }
            | Self::RevokeInboundBuildAuthority { authority } => &authority.history.resource_uid,
            Self::SessionRenewal { command } => &command.accepted().required.resource_uid,
        }
    }

    pub fn initialization(
        &self,
    ) -> Option<(
        &HistoryInitializationId,
        &HistoryContext,
        &ReplicaStorageIncarnationContext,
    )> {
        match self {
            Self::Initialize {
                accepted, storage, ..
            } => Some((
                &accepted.history_initialization_id,
                &accepted.history,
                storage,
            )),
            Self::Recover { .. }
            | Self::InstallInboundBuildAuthority { .. }
            | Self::RevokeInboundBuildAuthority { .. }
            | Self::SessionRenewal { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublicV1OperationCommand {
    pub target: ReplicaIdentity,
    pub intent: PublicOperationIntent,
}

impl PublicV1OperationCommand {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.intent.validate()?;
        if self.target.replica_id.value() <= 0
            || self.target.instance_id.is_empty()
            || self.target.agent_generation.is_empty()
        {
            return Err("public-v1 operation target is incomplete");
        }
        if let Some(crate::protocol::public_operations::PublicOperationProgram::Open {
            replica,
            ..
        }) = self.intent.program.as_ref()
            && replica != &self.target
        {
            return Err("public-v1 Open target differs from the exact replica");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
/// One fenced, idempotent authority command issued after a full observation.
pub enum ProtocolCommand {
    InitializeAgentStore(Box<InitializeAgentStore>),
    PublicV1History(Box<PublicV1HistoryCommand>),
    PublicV1Operation(Box<PublicV1OperationCommand>),
}

impl ProtocolCommand {
    pub fn effect_class(&self) -> EffectClass {
        match self {
            Self::InitializeAgentStore(_) => EffectClass::ConvergentEnsure,
            Self::PublicV1History(_) | Self::PublicV1Operation(_) => {
                EffectClass::ReconfigurationAction
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
/// Idempotent Kubernetes or status convergence performed by the controller.
pub enum KubernetesChange {
    EnsureReplicaSupport,
    EnsureReplicaScaffolding {
        replica_ids: Vec<ReplicaId>,
    },
    EnsureReplacementScaffolding {
        replica_id: ReplicaId,
        replacing: ReplicaIdentity,
    },
    DeleteReplicaScaffolding {
        pod_name: Option<String>,
        pod_uid: Option<PodUid>,
        pvc_name: Option<String>,
        pvc_uid: Option<PvcUid>,
    },
    DeleteReplicaEndpoint {
        identity: ReplicaIdentity,
    },
    DeleteExactPod {
        pod_name: String,
        pod_uid: PodUid,
    },
    DeleteExactService {
        name: String,
        uid: String,
        resource_version: String,
    },
    DeleteScaleDownResource {
        resource: ScaleDownResource,
        name: String,
        uid: String,
        resource_version: String,
    },
    EnsureWriteRoutingService,
    PersistStatus {
        status: Box<crate::protocol::types::AcceptedStatus>,
    },
    RemoveWriteRouting,
    PublishWriteRouting {
        primary: crate::protocol::types::ReplicaIdentity,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScaleDownResource {
    Endpoint,
    Pod,
    Pvc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SafetyChange {
    RemoveWriteRouting,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configuration_command_keeps_large_transition_evidence_off_stack() {
        assert!(
            std::mem::size_of::<EnsureConfiguration>() < 2_048,
            "EnsureConfiguration grew to {} bytes",
            std::mem::size_of::<EnsureConfiguration>()
        );
    }
}
