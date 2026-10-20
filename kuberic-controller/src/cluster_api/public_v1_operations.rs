#[cfg(feature = "runtime-test-bridge")]
use std::collections::BTreeMap;
#[cfg(feature = "runtime-test-bridge")]
use std::sync::{Arc, Mutex};

#[cfg(feature = "runtime-test-bridge")]
use kuberic_runtime::host::ReplicaHandle;
use kuberic_runtime::protocol::command::ProtocolCommand;
#[cfg(feature = "runtime-test-bridge")]
use kuberic_runtime::protocol::command::{
    PublicV1HistoryCommand, PublicV1OperationCommand, PublicV1SessionRenewalCommand,
};
#[cfg(feature = "runtime-test-bridge")]
use kuberic_runtime::protocol::public_operations::{
    PublicBuildAttemptReference, PublicConfiguration, PublicInboundBuildAuthority,
    PublicOperationClass, PublicV1SessionRenewalStatus,
};
use kuberic_runtime::protocol::public_operations::{
    PublicOperationIntent, PublicOperationProgram, PublicV1AcceptedStatus, PublicV1AgentReport,
    PublicV1CallKind,
};
use kuberic_runtime::protocol::types::{ConfigurationDescriptor, ConfigurationId};
#[cfg(feature = "runtime-test-bridge")]
use kuberic_runtime::protocol::types::{OperationId, ReplicaIdentity};

use super::PublicV1Scope;
#[cfg(feature = "runtime-test-bridge")]
use super::{
    PublicV1EvidenceApi, PublicV1EvidenceState, PublicV1HistoryApi, PublicV1SessionRenewalApi,
};
use crate::evaluator::public_v1_evidence::PublicV1AcceptedTransitionStatus;
#[cfg(feature = "runtime-test-bridge")]
use crate::evaluator::public_v1_evidence::PublicV1ResourceAuthority;

#[async_trait::async_trait]
pub trait PublicV1OperationApi: Send + Sync {
    async fn observe_public_v1_topology(
        &self,
        _scope: &PublicV1Scope,
    ) -> Result<Option<ConfigurationDescriptor>, String> {
        Ok(None)
    }

    async fn persist_public_v1_topology(
        &self,
        _scope: &PublicV1Scope,
        _expected: Option<&ConfigurationId>,
        _next: &ConfigurationDescriptor,
    ) -> Result<(), String> {
        Err("public-v1 topology persistence is unsupported".into())
    }

    async fn persist_public_v1_primary_unavailable(
        &self,
        _scope: &PublicV1Scope,
        _expected: &ConfigurationId,
        _unavailable: Option<(kuberic_runtime::protocol::types::ReplicaId, i64)>,
    ) -> Result<(), String> {
        Err("public-v1 primary availability persistence is unsupported".into())
    }

    async fn begin_public_v1_replacement(
        &self,
        _scope: &PublicV1Scope,
        _expected_topology: &ConfigurationId,
        _predecessor: &kuberic_runtime::protocol::types::ReplicaStorageIncarnationContext,
    ) -> Result<(), String> {
        Err("public-v1 replacement authority is unsupported".into())
    }

    async fn bind_public_v1_replacement(
        &self,
        _scope: &PublicV1Scope,
        _expected_operation: &kuberic_runtime::protocol::types::OperationId,
        _replacement: &kuberic_runtime::protocol::types::ReplicaStorageIncarnationContext,
    ) -> Result<(), String> {
        Err("public-v1 replacement binding is unsupported".into())
    }

    async fn complete_public_v1_replacement(
        &self,
        _scope: &PublicV1Scope,
        _expected_operation: &kuberic_runtime::protocol::types::OperationId,
    ) -> Result<(), String> {
        Err("public-v1 replacement completion is unsupported".into())
    }

    async fn begin_public_v1_operation(
        &self,
        _scope: &PublicV1Scope,
        _expected_authority: &PublicV1AcceptedStatus,
        _transition: &PublicV1AcceptedTransitionStatus,
        _command: &kuberic_runtime::protocol::command::PublicV1OperationCommand,
    ) -> Result<(), String> {
        Err("public-v1 operation origination is unsupported".into())
    }

    async fn contain_public_v1_operation(
        &self,
        _scope: &PublicV1Scope,
        _expected: &PublicV1AcceptedTransitionStatus,
        _command: &kuberic_runtime::protocol::command::ProtocolCommand,
    ) -> Result<(), String> {
        Ok(())
    }

    async fn supersede_public_v1_operation(
        &self,
        _scope: &PublicV1Scope,
        _expected: &PublicV1AcceptedTransitionStatus,
        _command: &kuberic_runtime::protocol::command::ProtocolCommand,
    ) -> Result<(), String> {
        Err("public-v1 operation supersession is unsupported".into())
    }

    async fn public_v1_operation_source_present(
        &self,
        _scope: &PublicV1Scope,
        _expected: &PublicV1AcceptedTransitionStatus,
    ) -> Result<bool, String> {
        Ok(true)
    }

    async fn observe_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
    ) -> Result<Option<PublicV1AcceptedTransitionStatus>, String>;

    async fn dispatch_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        command: &ProtocolCommand,
    ) -> Result<(), String>;

    async fn observe_public_v1_operation_report(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
    ) -> Result<PublicV1AgentReport, String>;

    async fn observe_public_v1_operation_source(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
    ) -> Result<crate::observation::RawAgentObservation, String> {
        self.observe_public_v1_operation_report(scope, expected)
            .await
            .map(|report| crate::observation::RawAgentObservation::PublicV1Report(Box::new(report)))
    }
}

#[derive(Clone)]
#[cfg(feature = "runtime-test-bridge")]
pub struct HostedPublicV1Replica {
    pub identity: ReplicaIdentity,
    pub replication_address: String,
    pub handle: ReplicaHandle,
}

#[cfg(feature = "runtime-test-bridge")]
pub struct HostedPublicV1Harness {
    replicas: Vec<HostedPublicV1Replica>,
    history: Mutex<Option<PublicV1AcceptedStatus>>,
    renewal: Mutex<Option<PublicV1SessionRenewalStatus>>,
    evidence: Arc<PublicV1EvidenceState>,
    dispatched: Mutex<Vec<ProtocolCommand>>,
    suboperations: Mutex<BTreeMap<(ReplicaIdentity, OperationId), PublicOperationIntent>>,
    suboperation_cut: Mutex<Option<usize>>,
}

#[cfg(feature = "runtime-test-bridge")]
impl HostedPublicV1Harness {
    pub fn new(
        replicas: Vec<HostedPublicV1Replica>,
        history: Option<PublicV1AcceptedStatus>,
        renewal: Option<PublicV1SessionRenewalStatus>,
        evidence: Arc<PublicV1EvidenceState>,
    ) -> Result<Self, String> {
        if replicas.is_empty() {
            return Err("hosted public-v1 adapter requires at least one replica".into());
        }
        for (index, replica) in replicas.iter().enumerate() {
            if replica.replication_address.is_empty()
                || replicas[..index]
                    .iter()
                    .any(|existing| existing.identity == replica.identity)
            {
                return Err("hosted public-v1 adapter contains an invalid replica".into());
            }
        }
        if let Some(history) = &history {
            history.validate_evidence().map_err(str::to_owned)?;
        }
        if let Some(renewal) = &renewal {
            renewal.validate().map_err(str::to_owned)?;
        }
        Ok(Self {
            replicas,
            history: Mutex::new(history),
            renewal: Mutex::new(renewal),
            evidence,
            dispatched: Mutex::new(Vec::new()),
            suboperations: Mutex::new(BTreeMap::new()),
            suboperation_cut: Mutex::new(None),
        })
    }

    pub fn dispatched(&self) -> Vec<ProtocolCommand> {
        self.dispatched.lock().unwrap().clone()
    }

    pub fn cut_after_hosted_suboperations(&self, count: usize) {
        *self.suboperation_cut.lock().unwrap() = Some(count.max(1));
    }

    pub async fn report(&self, identity: &ReplicaIdentity) -> Result<PublicV1AgentReport, String> {
        self.replica(identity)?
            .handle
            .public_v1_report()
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn reconstruct_hosted_configuration(
        &self,
        operation_id: &OperationId,
        preview_generation: u64,
        configuration: &kuberic_runtime::protocol::types::ConfigurationDescriptor,
    ) -> Result<(), String> {
        for member in &configuration.members {
            let report = self.report(&member.identity).await?;
            if !report.has_current_open_endpoint() {
                self.execute_authority_program(
                    &member.identity,
                    preview_generation,
                    OperationId::new(format!(
                        "{operation_id}-open-{}",
                        member.identity.replica_id
                    )),
                    PublicOperationProgram::Open {
                        replica: member.identity.clone(),
                        existing: report.open_mode
                            == kuberic_runtime::protocol::types::OpenMode::Existing,
                    },
                )
                .await?;
            }
        }
        self.install_configuration_on_all(operation_id, preview_generation, configuration)
            .await
    }

    fn replica(&self, identity: &ReplicaIdentity) -> Result<&HostedPublicV1Replica, String> {
        self.replicas
            .iter()
            .find(|replica| &replica.identity == identity)
            .ok_or_else(|| "hosted public-v1 command targets an unknown replica".to_string())
    }

    async fn public_configuration_for(
        &self,
        local: &ReplicaIdentity,
        configuration: &kuberic_runtime::protocol::types::ConfigurationDescriptor,
    ) -> Result<PublicConfiguration, String> {
        let mut descriptors = Vec::new();
        for member in configuration
            .members
            .iter()
            .filter(|member| &member.identity != local)
        {
            let remote = self.replica(&member.identity)?;
            let report = remote
                .handle
                .public_v1_report()
                .await
                .map_err(|error| error.to_string())?;
            descriptors.push(
                kuberic_runtime::protocol::public_operations::PublicReplicaDescriptor {
                    build_id: OperationId::default(),
                    identity: member.identity.clone(),
                    replication_address: remote.replication_address.clone(),
                    process_session_id: report.process_session_id,
                    storage_incarnation: Some(report.storage_incarnation),
                    role: member.role,
                    current_progress: report
                        .progress
                        .current()
                        .ok_or("native current progress unavailable")?,
                    catch_up_capability: report
                        .progress
                        .capability()
                        .ok_or("native catch-up capability unavailable")?,
                    must_catchup: false,
                },
            );
        }
        Ok(PublicConfiguration {
            current: configuration.clone(),
            previous: None,
            replicas: descriptors,
        })
    }

    async fn execute_authority_program(
        &self,
        target: &ReplicaIdentity,
        preview_generation: u64,
        operation_id: OperationId,
        program: PublicOperationProgram,
    ) -> Result<(), String> {
        let process_session_id = self.replica(target)?.handle.process_session_id();
        self.execute_authority_program_for_session(
            target,
            preview_generation,
            operation_id,
            process_session_id,
            program,
        )
        .await
    }

    async fn execute_authority_program_for_session(
        &self,
        target: &ReplicaIdentity,
        preview_generation: u64,
        operation_id: OperationId,
        process_session_id: kuberic_runtime::protocol::types::ProcessSessionId,
        program: PublicOperationProgram,
    ) -> Result<(), String> {
        let replica = self.replica(target)?;
        if replica.handle.process_session_id() != process_session_id {
            return Err("hosted suboperation changed its process session".into());
        }
        let key = (target.clone(), operation_id.clone());
        let existing = self.suboperations.lock().unwrap().get(&key).cloned();
        let intent = if let Some(existing) = existing {
            existing
        } else if let Some(durable) = replica
            .handle
            .public_v1_operation_intent(&operation_id)
            .await
            .map_err(|error| error.to_string())?
        {
            if durable.identity
                != kuberic_runtime::protocol::public_operations::PublicV1Identity::new(
                    preview_generation,
                )
                || durable.process_session_id != process_session_id
                || durable.class != PublicOperationClass::Authority
            {
                return Err("durable hosted suboperation changed accepted authority".into());
            }
            self.suboperations
                .lock()
                .unwrap()
                .entry(key)
                .or_insert_with(|| durable.clone())
                .clone()
        } else {
            let revision = replica
                .handle
                .public_v1_next_operation_revision()
                .await
                .map_err(|error| error.to_string())?;
            let intent = PublicOperationIntent {
                identity: kuberic_runtime::protocol::public_operations::PublicV1Identity::new(
                    preview_generation,
                ),
                operation_id,
                revision,
                process_session_id,
                class: PublicOperationClass::Authority,
                input_digest: program.digest(),
                replay_disposition: program.replay_disposition(),
                lifecycle: None,
                program: Some(program),
            };
            self.suboperations
                .lock()
                .unwrap()
                .entry(key)
                .or_insert_with(|| intent.clone())
                .clone()
        };
        replica
            .handle
            .execute_public_v1(intent.clone())
            .await
            .map_err(|error| {
                format!(
                    "hosted suboperation {} revision {} for {} failed: {error}",
                    intent.operation_id, intent.revision, target.replica_id
                )
            })?;
        let mut cut = self.suboperation_cut.lock().unwrap();
        if let Some(remaining) = cut.as_mut() {
            if *remaining == 1 {
                *cut = None;
                return Err("injected cut after hosted suboperation completion".into());
            }
            *remaining -= 1;
        }
        Ok(())
    }

    async fn install_configuration_on_all(
        &self,
        operation_id: &OperationId,
        preview_generation: u64,
        configuration: &kuberic_runtime::protocol::types::ConfigurationDescriptor,
    ) -> Result<(), String> {
        for member in &configuration.members {
            self.execute_authority_program(
                &member.identity,
                preview_generation,
                OperationId::new(format!(
                    "{operation_id}-epoch-{}",
                    member.identity.replica_id
                )),
                PublicOperationProgram::Epoch {
                    epoch: configuration.epoch,
                },
            )
            .await?;
            self.execute_authority_program(
                &member.identity,
                preview_generation,
                OperationId::new(format!(
                    "{operation_id}-role-{}",
                    member.identity.replica_id
                )),
                PublicOperationProgram::Role {
                    epoch: configuration.epoch,
                    role: member.role,
                },
            )
            .await?;
        }
        for member in &configuration.members {
            let program = PublicOperationProgram::Configuration(
                self.public_configuration_for(&member.identity, configuration)
                    .await?,
            );
            self.execute_authority_program(
                &member.identity,
                preview_generation,
                OperationId::new(format!(
                    "{operation_id}-configuration-{}",
                    member.identity.replica_id
                )),
                program,
            )
            .await?;
        }
        Ok(())
    }

    async fn dispatch_session(
        &self,
        command: &PublicV1SessionRenewalCommand,
    ) -> Result<(), String> {
        command.validate().map_err(str::to_owned)?;
        match command {
            PublicV1SessionRenewalCommand::CloseAccessAndIngress { accepted } => {
                self.replica(&accepted.required.predecessor.identity)?
                    .handle
                    .fence_public_v1_access()
                    .await;
            }
            PublicV1SessionRenewalCommand::UpdateEpochBarrier {
                accepted,
                survivor,
                epoch,
            } => {
                let program = PublicOperationProgram::Epoch { epoch: *epoch };
                self.execute_authority_program_for_session(
                    &survivor.identity,
                    accepted.required.history.protocol_generation.value(),
                    OperationId::new(format!(
                        "{}-barrier-{}",
                        accepted.required.operation_id, survivor.identity.replica_id
                    )),
                    survivor.process_session_id.clone(),
                    program,
                )
                .await?;
            }
            PublicV1SessionRenewalCommand::ReplayReplacementRole {
                accepted,
                process_session_id,
                epoch,
                role,
            } => {
                let replacement = accepted
                    .required
                    .replacement
                    .as_ref()
                    .ok_or_else(|| "session renewal lacks a replacement".to_string())?;
                let report = self.report(&replacement.identity).await?;
                if !report.has_current_open_endpoint() {
                    self.execute_authority_program(
                        &replacement.identity,
                        accepted.required.history.protocol_generation.value(),
                        OperationId::new(format!(
                            "{}-replacement-open",
                            accepted.required.operation_id
                        )),
                        PublicOperationProgram::Open {
                            replica: replacement.identity.clone(),
                            existing: report.open_mode
                                == kuberic_runtime::protocol::types::OpenMode::Existing,
                        },
                    )
                    .await?;
                }
                let program = PublicOperationProgram::Role {
                    epoch: *epoch,
                    role: *role,
                };
                self.execute_authority_program_for_session(
                    &replacement.identity,
                    accepted.required.history.protocol_generation.value(),
                    OperationId::new(format!("{}-role", accepted.required.operation_id)),
                    process_session_id.clone(),
                    program,
                )
                .await?;
            }
            PublicV1SessionRenewalCommand::InstallFreshConfiguration {
                accepted,
                configuration,
            } => {
                self.install_configuration_on_all(
                    &accepted.required.operation_id,
                    accepted.required.history.protocol_generation.value(),
                    configuration,
                )
                .await?;
            }
        }
        Ok(())
    }

    async fn persist_runtime_renewal(
        &self,
        expected: Option<&PublicV1SessionRenewalStatus>,
        next: &PublicV1SessionRenewalStatus,
    ) -> Result<(), String> {
        if let Some(replacement) = &next.required.replacement {
            self.replica(&replacement.identity)?
                .handle
                .persist_public_v1_session_renewal(expected, next)
                .await
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    async fn prepare_failover_predecessors(
        &self,
        operation: &PublicV1OperationCommand,
    ) -> Result<(), String> {
        let Some(lifecycle) = operation.intent.lifecycle.as_ref().filter(|lifecycle| {
            lifecycle.recipe
                == kuberic_runtime::protocol::public_operations::PublicLifecycleRecipe::
                    FailoverPromotion
        }) else {
            return Ok(());
        };
        for member in lifecycle
            .current
            .members
            .iter()
            .filter(|member| member.identity != operation.target)
        {
            self.replica(&member.identity)?
                .handle
                .fence_public_v1_access()
                .await;
            self.execute_authority_program(
                &member.identity,
                operation.intent.identity.generation,
                OperationId::new(format!(
                    "{}-predecessor-epoch-{}",
                    operation.intent.operation_id, member.identity.replica_id
                )),
                PublicOperationProgram::Epoch {
                    epoch: lifecycle.epoch,
                },
            )
            .await?;
            self.execute_authority_program(
                &member.identity,
                operation.intent.identity.generation,
                OperationId::new(format!(
                    "{}-predecessor-role-{}",
                    operation.intent.operation_id, member.identity.replica_id
                )),
                PublicOperationProgram::Role {
                    epoch: lifecycle.epoch,
                    role: member.role,
                },
            )
            .await?;
            let mut configuration = self
                .public_configuration_for(&member.identity, &lifecycle.current)
                .await?;
            configuration.previous = lifecycle.previous.clone();
            self.execute_authority_program(
                &member.identity,
                operation.intent.identity.generation,
                OperationId::new(format!(
                    "{}-predecessor-configuration-{}",
                    operation.intent.operation_id, member.identity.replica_id
                )),
                PublicOperationProgram::Configuration(configuration),
            )
            .await?;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
#[cfg(feature = "runtime-test-bridge")]
impl PublicV1OperationApi for HostedPublicV1Harness {
    async fn observe_public_v1_topology(
        &self,
        _scope: &PublicV1Scope,
    ) -> Result<Option<ConfigurationDescriptor>, String> {
        Ok(None)
    }

    async fn persist_public_v1_topology(
        &self,
        _scope: &PublicV1Scope,
        _expected: Option<&ConfigurationId>,
        next: &ConfigurationDescriptor,
    ) -> Result<(), String> {
        if next.configuration_id != next.expected_id() {
            return Err("hosted public-v1 topology is invalid".into());
        }
        Ok(())
    }

    async fn begin_public_v1_operation(
        &self,
        _scope: &PublicV1Scope,
        expected_authority: &PublicV1AcceptedStatus,
        transition: &PublicV1AcceptedTransitionStatus,
        command: &PublicV1OperationCommand,
    ) -> Result<(), String> {
        transition.validate()?;
        command.validate().map_err(str::to_owned)?;
        if &transition.evidence != expected_authority
            || !operation_matches_transition(transition, &command.intent)
            || command.target != transition.intent.source.storage_incarnation.replica
        {
            return Err("hosted public-v1 operation proposal is invalid".into());
        }
        self.dispatched
            .lock()
            .unwrap()
            .push(ProtocolCommand::PublicV1Operation(Box::new(
                command.clone(),
            )));
        Ok(())
    }

    async fn observe_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
    ) -> Result<Option<PublicV1AcceptedTransitionStatus>, String> {
        self.evidence.observe_public_v1_evidence_status(scope).await
    }

    async fn dispatch_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        command: &ProtocolCommand,
    ) -> Result<(), String> {
        if self
            .evidence
            .observe_public_v1_evidence_status(scope)
            .await?
            .as_ref()
            != Some(expected)
        {
            return Err("stale hosted public-v1 operation authority".into());
        }
        if scope.resource_uid != expected.evidence.history.resource_uid {
            return Err("hosted public-v1 operation scope differs from authority".into());
        }
        let ProtocolCommand::PublicV1Operation(operation) = command else {
            return Err("hosted public-v1 adapter received another operation command".into());
        };
        operation.validate().map_err(str::to_owned)?;
        if !operation_matches_transition(expected, &operation.intent) {
            return Err("hosted public-v1 operation changed accepted authority".into());
        }
        if operation.target != expected.intent.source.storage_incarnation.replica {
            return Err(
                "hosted public-v1 command target differs from accepted source incarnation".into(),
            );
        }
        Box::pin(self.prepare_failover_predecessors(operation)).await?;
        if let Some(PublicOperationProgram::Build(build)) = operation.intent.program.as_ref() {
            let target = expected
                .intent
                .target
                .as_ref()
                .ok_or_else(|| "hosted build lacks accepted target authority".to_string())?;
            if target.storage_incarnation != build.target_storage
                || build.replica != build.target_storage.replica
                || build.history.as_ref() != Some(&expected.intent.history)
                || expected.intent.current_configuration_id.is_none()
            {
                return Err("hosted build changed target, history, or configuration".into());
            }
            let target_replica = self.replica(&build.replica)?;
            if target_replica.replication_address != build.replication_address {
                return Err("hosted build changed target address".into());
            }
            if self.replica(&operation.target)?.replication_address != build.source_address {
                return Err("hosted build changed source address".into());
            }
            target_replica
                .handle
                .install_public_v1_inbound_build_authority(&PublicInboundBuildAuthority {
                    operation_id: expected.intent.operation_id.clone(),
                    operation_revision: expected.intent.revision,
                    attempt: PublicBuildAttemptReference {
                        resource_uid: expected.intent.history.resource_uid.clone(),
                        epoch: build.epoch,
                        build_id: build.attempt.clone(),
                        source: operation.target.clone(),
                        source_storage: expected.intent.source.storage_incarnation.clone(),
                        source_session: operation.intent.process_session_id.clone(),
                        target: build.replica.clone(),
                        target_storage: build.target_storage.clone(),
                        target_session: build.process_session_id.clone(),
                    },
                    history: expected.intent.history.clone(),
                    source_address: build.source_address.clone(),
                    current_configuration_id: expected
                        .intent
                        .current_configuration_id
                        .clone()
                        .expect("checked current configuration"),
                    target_address: build.replication_address.clone(),
                })
                .await
                .map_err(|error| error.to_string())?;
        }
        if let Some(PublicOperationProgram::Remove(build)) = operation.intent.program.as_ref() {
            self.execute_authority_program(
                &build.replica,
                operation.intent.identity.generation,
                OperationId::new(format!("{}-revoke-target", operation.intent.operation_id)),
                PublicOperationProgram::Role {
                    epoch: build.epoch,
                    role: kuberic_runtime::protocol::types::ReplicaRole::None,
                },
            )
            .await?;
        }
        if let Some(PublicOperationProgram::Swap {
            starting,
            refreshed,
            mode,
            controller_composed: true,
            ..
        }) = operation.intent.program.as_ref()
        {
            let catch_up = PublicOperationProgram::CatchUp {
                configuration: starting.clone(),
                mode: *mode,
            };
            let catch_up_revision = operation.intent.revision.checked_sub(1).ok_or_else(|| {
                "controller-composed swap lacks a pre-catch-up revision".to_string()
            })?;
            self.replica(&operation.target)?
                .handle
                .execute_public_v1(PublicOperationIntent {
                    identity: operation.intent.identity.clone(),
                    operation_id: OperationId::new(format!(
                        "{}-first-catch-up",
                        operation.intent.operation_id
                    )),
                    revision: catch_up_revision,
                    process_session_id: operation.intent.process_session_id.clone(),
                    class: PublicOperationClass::Authority,
                    input_digest: catch_up.digest(),
                    replay_disposition: catch_up.replay_disposition(),
                    lifecycle: None,
                    program: Some(catch_up),
                })
                .await
                .map_err(|error| error.to_string())?;
            for member in refreshed
                .current
                .members
                .iter()
                .filter(|member| member.identity != operation.target)
            {
                self.replica(&member.identity)?
                    .handle
                    .fence_public_v1_access()
                    .await;
                self.execute_authority_program(
                    &member.identity,
                    operation.intent.identity.generation,
                    OperationId::new(format!(
                        "{}-target-epoch-{}",
                        operation.intent.operation_id, member.identity.replica_id
                    )),
                    PublicOperationProgram::Epoch {
                        epoch: refreshed.current.epoch,
                    },
                )
                .await?;
                self.execute_authority_program(
                    &member.identity,
                    operation.intent.identity.generation,
                    OperationId::new(format!(
                        "{}-target-role-{}",
                        operation.intent.operation_id, member.identity.replica_id
                    )),
                    PublicOperationProgram::Role {
                        epoch: refreshed.current.epoch,
                        role: member.role,
                    },
                )
                .await?;
                let configuration = PublicOperationProgram::Configuration(
                    self.public_configuration_for(&member.identity, &refreshed.current)
                        .await?,
                );
                self.execute_authority_program(
                    &member.identity,
                    operation.intent.identity.generation,
                    OperationId::new(format!(
                        "{}-target-configuration-{}",
                        operation.intent.operation_id, member.identity.replica_id
                    )),
                    configuration,
                )
                .await?;
            }
        }
        self.replica(&operation.target)?
            .handle
            .execute_public_v1_command(command.clone())
            .await
            .map_err(|error| error.to_string())?;
        if let Some(PublicOperationProgram::Swap {
            refreshed, epoch, ..
        }) = operation.intent.program.as_ref()
        {
            if refreshed.current.epoch != *epoch {
                return Err("hosted switchover changed refreshed epoch".into());
            }
            let target =
                expected.intent.target.as_ref().ok_or_else(|| {
                    "hosted switchover lacks accepted target authority".to_string()
                })?;
            let target_id = target.storage_incarnation.replica.replica_id;
            let members = refreshed
                .current
                .members
                .iter()
                .cloned()
                .map(|mut member| {
                    member.role = if member.identity.replica_id == target_id {
                        kuberic_runtime::protocol::types::ReplicaRole::Primary
                    } else {
                        kuberic_runtime::protocol::types::ReplicaRole::ActiveSecondary
                    };
                    member
                })
                .collect();
            let promoted = kuberic_runtime::protocol::types::ConfigurationDescriptor::new(
                *epoch,
                target_id,
                members,
                refreshed.current.write_quorum,
            );
            self.install_configuration_on_all(
                &operation.intent.operation_id,
                expected.intent.history.protocol_generation.value(),
                &promoted,
            )
            .await?;
        }
        self.dispatched.lock().unwrap().push(command.clone());
        Ok(())
    }

    async fn observe_public_v1_operation_report(
        &self,
        _scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
    ) -> Result<PublicV1AgentReport, String> {
        self.report(&expected.intent.source.storage_incarnation.replica)
            .await
    }
}

#[async_trait::async_trait]
#[cfg(feature = "runtime-test-bridge")]
impl PublicV1HistoryApi for HostedPublicV1Harness {
    async fn observe_public_v1_status(
        &self,
        _scope: &PublicV1Scope,
    ) -> Result<Option<PublicV1AcceptedStatus>, String> {
        Ok(self.history.lock().unwrap().clone())
    }

    async fn persist_public_v1_status(
        &self,
        _scope: &PublicV1Scope,
        expected: Option<&PublicV1AcceptedStatus>,
        next: &PublicV1AcceptedStatus,
    ) -> Result<(), String> {
        next.validate_evidence().map_err(str::to_owned)?;
        let mut current = self.history.lock().unwrap();
        if current.as_ref() != expected {
            return Err("stale hosted public-v1 history status".into());
        }
        *current = Some(next.clone());
        Ok(())
    }

    async fn dispatch_public_v1_history(
        &self,
        _scope: &PublicV1Scope,
        expected: &PublicV1AcceptedStatus,
        command: &ProtocolCommand,
    ) -> Result<(), String> {
        if self.history.lock().unwrap().as_ref() != Some(expected) {
            return Err("stale hosted public-v1 history dispatch".into());
        }
        let ProtocolCommand::PublicV1History(public_command) = command else {
            return Err("hosted public-v1 history received another command".into());
        };
        public_command.validate().map_err(str::to_owned)?;
        let replica = self.replica(public_command.target())?;
        self.dispatched.lock().unwrap().push(command.clone());
        match public_command.as_ref() {
            PublicV1HistoryCommand::Initialize {
                accepted, storage, ..
            } => {
                replica
                    .handle
                    .initialize_public_v1_history(
                        &accepted.history_initialization_id,
                        &accepted.history,
                        storage,
                    )
                    .await
                    .map_err(|error| error.to_string())?;
            }
            PublicV1HistoryCommand::Recover { intent, .. } => {
                replica
                    .handle
                    .execute_public_v1(intent.as_ref().clone())
                    .await
                    .map_err(|error| error.to_string())?;
            }
            PublicV1HistoryCommand::InstallInboundBuildAuthority { authority } => {
                replica
                    .handle
                    .install_public_v1_inbound_build_authority(authority)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            PublicV1HistoryCommand::RevokeInboundBuildAuthority { authority } => {
                replica
                    .handle
                    .revoke_public_v1_inbound_build_authority(authority)
                    .await
                    .map_err(|error| error.to_string())?;
            }
            PublicV1HistoryCommand::SessionRenewal { .. } => {
                return Err("session-renewal command used the history API".into());
            }
        }
        Ok(())
    }
}

#[async_trait::async_trait]
#[cfg(feature = "runtime-test-bridge")]
impl PublicV1SessionRenewalApi for HostedPublicV1Harness {
    async fn observe_public_v1_session_renewal(
        &self,
        _scope: &PublicV1Scope,
    ) -> Result<Option<PublicV1SessionRenewalStatus>, String> {
        Ok(self.renewal.lock().unwrap().clone())
    }

    async fn persist_public_v1_session_renewal(
        &self,
        _scope: &PublicV1Scope,
        expected: Option<&PublicV1SessionRenewalStatus>,
        next: &PublicV1SessionRenewalStatus,
    ) -> Result<(), String> {
        next.validate().map_err(str::to_owned)?;
        if self.renewal.lock().unwrap().as_ref() != expected {
            return Err("stale hosted public-v1 session-renewal status".into());
        }
        self.persist_runtime_renewal(expected, next).await?;
        let mut current = self.renewal.lock().unwrap();
        if current.as_ref() != expected {
            return Err("stale hosted public-v1 session-renewal status".into());
        }
        *current = Some(next.clone());
        Ok(())
    }

    async fn dispatch_public_v1_session_renewal(
        &self,
        _scope: &PublicV1Scope,
        expected: &PublicV1SessionRenewalStatus,
        command: &ProtocolCommand,
    ) -> Result<(), String> {
        if self.renewal.lock().unwrap().as_ref() != Some(expected) {
            return Err("stale hosted public-v1 session-renewal dispatch".into());
        }
        let ProtocolCommand::PublicV1History(public_command) = command else {
            return Err("hosted session renewal received another command".into());
        };
        let PublicV1HistoryCommand::SessionRenewal {
            command: renewal_command,
        } = public_command.as_ref()
        else {
            return Err("hosted session renewal received a history command".into());
        };
        if renewal_command.accepted() != expected {
            return Err("hosted session renewal changed accepted authority".into());
        }
        self.dispatch_session(renewal_command).await?;
        let next = match renewal_command.as_ref() {
            PublicV1SessionRenewalCommand::CloseAccessAndIngress { accepted } => {
                accepted.close_access()
            }
            PublicV1SessionRenewalCommand::UpdateEpochBarrier {
                accepted,
                survivor,
                epoch,
            } => accepted.record_epoch_updated(survivor, *epoch),
            PublicV1SessionRenewalCommand::ReplayReplacementRole {
                accepted,
                process_session_id,
                epoch,
                role,
            } => accepted.replay_replacement_role(process_session_id, *epoch, *role),
            PublicV1SessionRenewalCommand::InstallFreshConfiguration {
                accepted,
                configuration,
            } => accepted.install_fresh_configuration(configuration),
        }
        .map_err(str::to_owned)?;
        self.persist_runtime_renewal(Some(expected), &next).await?;
        *self.renewal.lock().unwrap() = Some(next);
        self.dispatched.lock().unwrap().push(command.clone());
        Ok(())
    }
}

#[async_trait::async_trait]
#[cfg(feature = "runtime-test-bridge")]
impl PublicV1EvidenceApi for HostedPublicV1Harness {
    async fn observe_public_v1_evidence_status(
        &self,
        scope: &PublicV1Scope,
    ) -> Result<Option<PublicV1AcceptedTransitionStatus>, String> {
        self.evidence.observe_public_v1_evidence_status(scope).await
    }

    async fn persist_public_v1_evidence_status(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        next: &PublicV1AcceptedStatus,
    ) -> Result<(), String> {
        self.evidence
            .persist_public_v1_evidence_status(scope, expected, next)
            .await
    }

    async fn accept_public_v1_completion(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &kuberic_runtime::protocol::public_operations::PublicV1OperationCompletion,
    ) -> Result<(), String> {
        self.evidence
            .accept_public_v1_completion(scope, expected, completion)
            .await
    }

    async fn retire_public_v1_incarnation(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &kuberic_runtime::protocol::public_operations::PublicV1OperationCompletion,
        target: &PublicV1ResourceAuthority,
    ) -> Result<(), String> {
        self.evidence
            .retire_public_v1_incarnation(scope, expected, completion, target)
            .await
    }

    async fn provision_public_v1_replacement(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &kuberic_runtime::protocol::public_operations::PublicV1OperationCompletion,
        target: &PublicV1ResourceAuthority,
        rebuild: &kuberic_runtime::protocol::public_operations::CanonicalReplicaRebuildRequired,
    ) -> Result<(), String> {
        self.evidence
            .provision_public_v1_replacement(scope, expected, completion, target, rebuild)
            .await
    }
}

pub(crate) fn operation_matches_transition(
    expected: &PublicV1AcceptedTransitionStatus,
    intent: &PublicOperationIntent,
) -> bool {
    if intent.operation_id != expected.intent.operation_id
        || intent.revision != expected.intent.revision
        || intent.process_session_id != expected.intent.process_session_id
        || intent.input_digest != expected.intent.input_digest
        || intent.replay_disposition != expected.intent.replay_disposition
        || !intent_history_matches(intent, expected.intent.history.data_loss_number)
    {
        return false;
    }
    let Some(program) = intent.program.as_ref() else {
        return matches!(
            expected.intent.call_kind,
            PublicV1CallKind::InitialPrimary
                | PublicV1CallKind::FailoverPromotion
                | PublicV1CallKind::SecondaryEpochAdvance
        );
    };
    let (call, previous, current, mode, specific) = match program {
        PublicOperationProgram::Open { .. } => (PublicV1CallKind::Open, None, None, None, None),
        PublicOperationProgram::Role { .. } => (PublicV1CallKind::Role, None, None, None, None),
        PublicOperationProgram::Epoch { .. } => (PublicV1CallKind::Epoch, None, None, None, None),
        PublicOperationProgram::Configuration(configuration) => (
            PublicV1CallKind::Configuration,
            configuration
                .previous
                .as_ref()
                .map(|configuration| configuration.configuration_id.clone()),
            Some(configuration.current.configuration_id.clone()),
            None,
            None,
        ),
        PublicOperationProgram::CatchUp {
            configuration,
            mode,
        } => (
            PublicV1CallKind::CatchUp,
            configuration
                .previous
                .as_ref()
                .map(|configuration| configuration.configuration_id.clone()),
            Some(configuration.current.configuration_id.clone()),
            Some(*mode),
            None,
        ),
        PublicOperationProgram::Progress { .. } => {
            (PublicV1CallKind::Progress, None, None, None, None)
        }
        PublicOperationProgram::Swap {
            refreshed,
            mode,
            catch_up_specific_quorum,
            ..
        } => (
            PublicV1CallKind::Swap,
            refreshed
                .previous
                .as_ref()
                .map(|configuration| configuration.configuration_id.clone()),
            Some(refreshed.current.configuration_id.clone()),
            Some(*mode),
            Some(*catch_up_specific_quorum),
        ),
        PublicOperationProgram::Build(build) => (
            PublicV1CallKind::Build,
            None,
            build.current_configuration_id.clone(),
            None,
            None,
        ),
        PublicOperationProgram::Remove(_) | PublicOperationProgram::RetireIdleReplica { .. } => {
            (PublicV1CallKind::Remove, None, None, None, None)
        }
        PublicOperationProgram::Close => (PublicV1CallKind::Close, None, None, None, None),
        PublicOperationProgram::Abort => (PublicV1CallKind::Abort, None, None, None, None),
    };
    call == expected.intent.call_kind
        && previous == expected.intent.previous_configuration_id
        && current == expected.intent.current_configuration_id
        && mode == expected.intent.catch_up_mode
        && specific == expected.intent.catch_up_specific_quorum
}

fn intent_history_matches(intent: &PublicOperationIntent, data_loss_number: i64) -> bool {
    let epoch_matches =
        |epoch: kuberic_runtime::protocol::types::Epoch| epoch.data_loss_number == data_loss_number;
    match intent.program.as_ref() {
        Some(PublicOperationProgram::Role { epoch, .. })
        | Some(PublicOperationProgram::Epoch { epoch }) => epoch_matches(*epoch),
        Some(PublicOperationProgram::Configuration(configuration))
        | Some(PublicOperationProgram::CatchUp { configuration, .. }) => {
            epoch_matches(configuration.current.epoch)
                && configuration
                    .previous
                    .as_ref()
                    .is_none_or(|previous| epoch_matches(previous.epoch))
        }
        Some(PublicOperationProgram::Swap {
            starting,
            refreshed,
            epoch,
            ..
        }) => {
            epoch_matches(*epoch)
                && epoch_matches(starting.current.epoch)
                && epoch_matches(refreshed.current.epoch)
                && starting
                    .previous
                    .as_ref()
                    .is_none_or(|previous| epoch_matches(previous.epoch))
                && refreshed
                    .previous
                    .as_ref()
                    .is_none_or(|previous| epoch_matches(previous.epoch))
        }
        Some(PublicOperationProgram::Build(build))
        | Some(PublicOperationProgram::Remove(build)) => build
            .history
            .as_ref()
            .is_some_and(|history| history.data_loss_number == data_loss_number),
        Some(
            PublicOperationProgram::Open { .. }
            | PublicOperationProgram::Progress { .. }
            | PublicOperationProgram::RetireIdleReplica { .. }
            | PublicOperationProgram::Close
            | PublicOperationProgram::Abort,
        )
        | None => true,
    }
}
