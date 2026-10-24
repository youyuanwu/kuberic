//! Exact host authority for the classic V1 logical carrier.
//!
//! Native routing identifiers are representations, not credentials. Admission
//! checks the full process/storage/history/address binding against the live
//! journal before handing a message to a native owner.

use serde::{Deserialize, Serialize};

use crate::protocol::public_operations::{
    PublicConfiguration, PublicInboundBuildAuthority, PublicOperationProgram,
};
use crate::protocol::types::{
    ConfigurationId, Epoch, HistoryContext, ProcessSessionId, ReplicaIdentity,
    ReplicaStorageIncarnationContext,
};
use crate::{Result, RuntimeError};

use super::state::{AgentState, PublicOperationDisposition, PublicOperationStage};

pub(crate) fn recovery_observations_pending(state: &AgentState) -> bool {
    state
        .public_v1_operation_state
        .as_ref()
        .is_some_and(|preview| {
            preview.data_loss_recovery.as_ref().is_some_and(|recovery| {
                preview.history.as_ref() != Some(&recovery.proposed_history)
            })
        })
}

pub(crate) fn transport_history<'a>(
    state: &'a AgentState,
    process: &ProcessSessionId,
) -> Result<&'a HistoryContext> {
    use crate::protocol::public_operations::PublicDataLossRecoveryStage;
    let preview = state
        .public_v1_operation_state
        .as_ref()
        .ok_or(RuntimeError::AuthorityNotAdmitted)?;
    let admitted = preview
        .history
        .as_ref()
        .ok_or(RuntimeError::AuthorityNotAdmitted)?;
    let Some(recovery) = &preview.data_loss_recovery else {
        return Ok(admitted);
    };
    if admitted == &recovery.proposed_history
        || !matches!(
            recovery.stage,
            PublicDataLossRecoveryStage::HistoryPending
                | PublicDataLossRecoveryStage::ConfigurationPending
        )
    {
        return Ok(admitted);
    }
    let record = preview
        .current_operation
        .as_ref()
        .and_then(|id| preview.operations.get(id))
        .ok_or(RuntimeError::AuthorityNotAdmitted)?;
    let input = record
        .intent
        .lifecycle
        .as_ref()
        .and_then(|input| input.data_loss_recovery.as_ref())
        .ok_or(RuntimeError::AuthorityNotAdmitted)?;
    if record.stage != PublicOperationStage::Running
        || record.superseded_by.is_some()
        || record.intent.process_session_id != *process
        || input.attempt != recovery.attempt
        || input.authorization != recovery.logical_authorization()
        || preview.data_loss_authorization.as_ref() != Some(&input.authorization)
        || admitted != &input.authorization.predecessor
        || preview.storage_incarnation.as_ref() != Some(&recovery.provisional_primary)
    {
        return Err(RuntimeError::AuthorityNotAdmitted);
    }
    Ok(&recovery.proposed_history)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeEndpoint {
    pub(crate) identity: ReplicaIdentity,
    pub(crate) process_session_id: ProcessSessionId,
    pub(crate) storage: ReplicaStorageIncarnationContext,
    pub(crate) address: String,
}

impl NativeEndpoint {
    pub(crate) fn validate(&self) -> Result<()> {
        self.storage
            .validate()
            .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))?;
        if self.identity.replica_id.value() <= 0
            || self.identity.instance_id.is_empty()
            || self.identity.agent_generation.is_empty()
            || self.process_session_id.is_empty()
            || self.storage.replica != self.identity
            || self.address.is_empty()
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NativeBinding {
    pub(crate) history: HistoryContext,
    pub(crate) epoch: Epoch,
    pub(crate) current_configuration_id: ConfigurationId,
    pub(crate) previous_configuration_id: Option<ConfigurationId>,
    pub(crate) primary: NativeEndpoint,
    pub(crate) secondary: NativeEndpoint,
    pub(crate) build: Option<PublicInboundBuildAuthority>,
}

impl NativeBinding {
    pub(crate) fn validate_for_state(
        &self,
        state: &AgentState,
        process: &ProcessSessionId,
        address: &str,
    ) -> Result<()> {
        self.primary.validate()?;
        self.secondary.validate()?;
        self.history
            .validate()
            .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))?;
        if self.primary.identity == self.secondary.identity
            || self.epoch.data_loss_number != self.history.data_loss_number
            || self.epoch.configuration_number < 0
            || self.current_configuration_id.is_empty()
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let preview = state
            .public_v1_operation_state
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let local = if state.identity.local_identity == self.primary.identity {
            &self.primary
        } else if state.identity.local_identity == self.secondary.identity {
            &self.secondary
        } else {
            return Err(RuntimeError::AuthorityNotAdmitted);
        };
        if preview.terminal
            || state.identity.resource_uid != self.history.resource_uid
            || transport_history(state, process)? != &self.history
            || preview.storage_incarnation.as_ref() != Some(&local.storage)
            || &local.process_session_id != process
            || local.address != address
            || preview.retired_storage_incarnations.iter().any(|storage| {
                storage == &self.primary.storage || storage == &self.secondary.storage
            })
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if preview
            .operations
            .values()
            .filter(|record| {
                matches!(
                    record.intent.class,
                    crate::protocol::public_operations::PublicOperationClass::Authority
                        | crate::protocol::public_operations::PublicOperationClass::PlannedSwap
                )
            })
            .max_by_key(|record| record.intent.revision)
            .is_some_and(|record| record.intent.process_session_id != *process)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        if let Some(authority) = &self.build {
            authority
                .validate()
                .map_err(|error| RuntimeError::AuthorityMismatch(error.into()))?;
            let attempt = &authority.attempt;
            if authority.history != self.history
                || authority.current_configuration_id != self.current_configuration_id
                || authority.source_address != self.primary.address
                || authority.target_address != self.secondary.address
                || attempt.epoch != self.epoch
                || attempt.source != self.primary.identity
                || attempt.source_storage != self.primary.storage
                || attempt.source_session != self.primary.process_session_id
                || attempt.target != self.secondary.identity
                || attempt.target_storage != self.secondary.storage
                || attempt.target_session != self.secondary.process_session_id
                || attempt.resource_uid != self.history.resource_uid
            {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            if local == &self.secondary {
                if preview.current_inbound_build_authority.as_ref() == Some(authority) {
                    return Ok(());
                }
            } else if preview.operations.values().any(|record| {
                record.intent.operation_id == authority.operation_id
                    && record.intent.revision == authority.operation_revision
                    && record.intent.process_session_id == *process
                    && record.accepted_history.as_ref() == Some(&self.history)
                    && record.accepted_storage.as_ref() == Some(&local.storage)
                    && !matches!(
                        record.disposition,
                        Some(
                            PublicOperationDisposition::Failed(_)
                                | PublicOperationDisposition::Cancelled
                                | PublicOperationDisposition::Ambiguous(_)
                        )
                    )
                    && matches!(
                        record.intent.program.as_ref(),
                        Some(PublicOperationProgram::Build(build))
                            if build.attempt == attempt.build_id
                                && build.replica == self.secondary.identity
                                && build.target_storage == self.secondary.storage
                                && build.process_session_id == self.secondary.process_session_id
                                && build.source_address == self.primary.address
                                && build.replication_address == self.secondary.address
                                && build.history.as_ref() == Some(&self.history)
                    )
            }) {
                return Ok(());
            }
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let latest = preview
            .operations
            .values()
            .filter(|record| {
                record.intent.process_session_id == *process
                    && (record.accepted_history.as_ref() == Some(&self.history)
                        || record.intent.lifecycle.as_ref().is_some_and(|input| {
                            input.data_loss_recovery.as_ref().is_some_and(|recovery| {
                                recovery.authorization.proposed == self.history
                                    && preview.current_operation.as_ref()
                                        == Some(&record.intent.operation_id)
                            })
                        }))
                    && record.accepted_storage.as_ref() == Some(&local.storage)
                    && !matches!(
                        record.disposition,
                        Some(
                            PublicOperationDisposition::Failed(_)
                                | PublicOperationDisposition::Cancelled
                                | PublicOperationDisposition::Ambiguous(_)
                        )
                    )
                    && (record.stage != PublicOperationStage::Completed
                        || record.disposition == Some(PublicOperationDisposition::Succeeded))
            })
            .filter_map(|record| {
                let configuration = record
                    .intent
                    .lifecycle
                    .as_ref()
                    .map(|lifecycle| lifecycle.configuration())
                    .or_else(|| match record.intent.program.as_ref() {
                        Some(
                            PublicOperationProgram::Configuration(configuration)
                            | PublicOperationProgram::CatchUp { configuration, .. },
                        ) => Some(configuration.clone()),
                        Some(PublicOperationProgram::Swap {
                            starting,
                            refreshed,
                            ..
                        }) => {
                            let selected = super::public_lifecycle::instruction_selected(
                                record,
                                super::state::PublicInstruction::RefreshedConfiguration,
                            );
                            Some(if selected {
                                refreshed.clone()
                            } else {
                                starting.clone()
                            })
                        }
                        _ => None,
                    })?;
                Some((record.intent.revision, configuration))
            })
            .max_by_key(|(revision, _)| *revision);
        if latest
            .is_some_and(|(_, configuration)| self.matches_configuration(&configuration, local))
        {
            Ok(())
        } else {
            Err(RuntimeError::AuthorityNotAdmitted)
        }
    }

    fn matches_configuration(
        &self,
        configuration: &PublicConfiguration,
        local: &NativeEndpoint,
    ) -> bool {
        use crate::protocol::types::ReplicaRole;
        let current = &configuration.current;
        if current.epoch != self.epoch
            || current.configuration_id != self.current_configuration_id
            || configuration
                .previous
                .as_ref()
                .map(|previous| &previous.configuration_id)
                != self.previous_configuration_id.as_ref()
            || current.primary_id != self.primary.identity.replica_id
            || !current.members.iter().any(|member| {
                member.identity == self.primary.identity && member.role == ReplicaRole::Primary
            })
            || !(current.members.iter().any(|member| {
                member.identity == self.secondary.identity
                    && member.role == ReplicaRole::ActiveSecondary
            }) || configuration.previous.as_ref().is_some_and(|previous| {
                previous.members.iter().any(|member| {
                    member.identity == self.secondary.identity
                        && matches!(
                            member.role,
                            ReplicaRole::Primary | ReplicaRole::ActiveSecondary
                        )
                })
            }))
        {
            return false;
        }
        let remote = if local == &self.primary {
            &self.secondary
        } else {
            &self.primary
        };
        configuration.replicas.iter().any(|replica| {
            replica.identity == remote.identity
                && replica.process_session_id == remote.process_session_id
                && replica.storage_incarnation.as_ref() == Some(&remote.storage)
                && replica.replication_address == remote.address
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::public_operations::PublicReplicaDescriptor;
    use crate::protocol::types::{
        AgentGeneration, ConfigurationDescriptor, ConfigurationMember, OperationId, PodUid,
        PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaInstanceId, ReplicaRole,
        ReplicaStorageIdentity, ResourceUid, StorageGeneration, derive_bootstrap_history,
        derive_history_initialization_id,
    };

    fn endpoint(id: i64) -> NativeEndpoint {
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(id),
            instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
            agent_generation: AgentGeneration::new(format!("generation-{id}")),
        };
        NativeEndpoint {
            storage: ReplicaStorageIncarnationContext {
                replica: identity.clone(),
                storage: ReplicaStorageIdentity::Persistent {
                    pod_uid: PodUid::new(format!("pod-{id}")),
                    pvc_uid: PvcUid::new(format!("pvc-{id}")),
                },
                storage_generation: StorageGeneration::new(format!("storage-{id}")),
            },
            identity,
            process_session_id: ProcessSessionId::new(format!("session-{id}")),
            address: format!("http://127.0.0.1:{}", 41000 + id),
        }
    }

    #[test]
    fn failover_binding_preserves_previous_primary_role_and_exact_endpoint() {
        let old = endpoint(1);
        let local = endpoint(2);
        let member = |endpoint: &NativeEndpoint, role| ConfigurationMember {
            identity: endpoint.identity.clone(),
            role,
        };
        let previous = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            old.identity.replica_id,
            vec![
                member(&old, ReplicaRole::Primary),
                member(&local, ReplicaRole::ActiveSecondary),
            ],
            2,
        );
        let current = ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            local.identity.replica_id,
            vec![member(&local, ReplicaRole::Primary)],
            1,
        );
        let configuration = PublicConfiguration {
            current: current.clone(),
            previous: Some(previous.clone()),
            replicas: vec![PublicReplicaDescriptor {
                is_up: true,
                is_ready: true,
                build_id: OperationId::default(),
                identity: old.identity.clone(),
                process_session_id: old.process_session_id.clone(),
                storage_incarnation: Some(old.storage.clone()),
                replication_address: old.address.clone(),
                role: ReplicaRole::Primary,
                current_progress: -1,
                catch_up_capability: -1,
                must_catchup: false,
            }],
        };
        configuration.validate().unwrap();
        let resource = ResourceUid::new("binding-test");
        let generation = PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource, generation);
        let binding = NativeBinding {
            history: derive_bootstrap_history(&resource, generation, &initialization),
            epoch: current.epoch,
            current_configuration_id: current.configuration_id.clone(),
            previous_configuration_id: Some(previous.configuration_id.clone()),
            primary: local.clone(),
            secondary: old,
            build: None,
        };
        assert!(binding.matches_configuration(&configuration, &local));
        for mutation in 0..6 {
            let mut invalid = configuration.clone();
            match mutation {
                0 => invalid.previous = None,
                1 => {
                    invalid.previous.as_mut().unwrap().members[0].role = ReplicaRole::IdleSecondary
                }
                2 => invalid.current.primary_id = ReplicaId::new(3),
                3 => {
                    invalid.replicas[0].process_session_id =
                        ProcessSessionId::new("another-process")
                }
                4 => {
                    invalid.replicas[0]
                        .storage_incarnation
                        .as_mut()
                        .unwrap()
                        .storage_generation = StorageGeneration::new("another-storage")
                }
                5 => invalid.replicas[0].replication_address = "http://127.0.0.1:49999".into(),
                _ => unreachable!(),
            }
            assert!(
                !binding.matches_configuration(&invalid, &local),
                "mutation {mutation}"
            );
        }
    }
}
