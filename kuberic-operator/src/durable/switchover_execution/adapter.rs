use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
};

use async_trait::async_trait;
use k8s_openapi::api::core::v1::Pod;
use kuberic_core::{
    error::KubericError,
    types::{ReplicaId, ReplicaInstanceId},
};
#[cfg(test)]
use kuberic_durable_execution::ExecutionContract;
use kuberic_durable_execution::{
    ActivityObservation, ActivitySpec, CheckpointError, CheckpointLimits, ExactBytes, ExecutionId,
    ExecutionSpec, LogicalActivityId, PreparedActivityError, PreparedActivityResolver,
    TerminalOutcome, decode_activity_result, encode_activity_result,
};

use crate::{
    cluster_api::ClusterApi,
    crd::KubericSet,
    durable::{
        OperationObservations,
        checkpoint_store::{
            CheckpointMeasurementDecoder, DurableActivityAccounting, DurableActivityClass,
            MeasuredDurableCheckpointStore,
        },
        effects::{
            DispatchFailureDisposition, classify_dispatch_failure, execute_label_command,
            execute_replica_command,
        },
        runner::{
            DurableAdapterBoundary, DurableAdapterWait, DurableCheckpointDisposition,
            DurableOperationAdapter,
        },
        workflow_host::DurablePermitGuard,
    },
};

#[cfg(test)]
use super::workflow::DirectSwitchoverWorkflow;
use super::{
    SwitchoverRunnerContext, SwitchoverWorkflowInput,
    activities::{
        ALL_DIRECT_ACTIVITY_IDENTITIES, AttestCompensatedTopologyActivity,
        AttestCompensatedTopologyOutput, AttestTargetTopologyActivity, AttestTargetTopologyOutput,
        DIRECT_SWITCHOVER_CONTRACT_VERSION, DirectActivityAccounting,
    },
    collect_switchover_runner_context, encode_execution_id,
    model::{DirectSwitchoverDefinition, is_valid_compensation_topology, same_topology},
    prepare::{DirectActivity, DirectEvaluation},
    quarantine::{DirectQuarantineOutcome, resolve_direct_quarantine},
    workflow::DirectSwitchoverTerminalRecord,
};

pub const DIRECT_SWITCHOVER_MAX_ACTIVITY_RECORDS: usize =
    2 * crate::crd::KUBERIC_MAX_REPLICAS as usize + 15;
pub const DIRECT_SWITCHOVER_MAX_TRANSITION_FUEL: usize = 64;
pub const DIRECT_SWITCHOVER_MAX_RUNNER_FUEL: usize = 32;
pub const DIRECT_SWITCHOVER_MAX_WORKFLOW_INPUT_BYTES: usize = 4_096;
pub const DIRECT_SWITCHOVER_MAX_ACTIVE_ENCODED_BYTES: usize = 512 * 1_024;
pub const DIRECT_SWITCHOVER_MAX_TERMINAL_ENCODED_BYTES: usize = 16 * 1_024;
pub const DIRECT_SWITCHOVER_MAX_TERMINAL_PAYLOAD_BYTES: u64 = 4_096;
pub const DIRECT_SWITCHOVER_MAX_ERROR_BYTES: usize = 512;

pub struct DirectSwitchoverPreparedActivityResolver {
    definition: DirectSwitchoverDefinition,
    observations: OperationObservations,
    addressed_instances: BTreeMap<ReplicaId, ReplicaInstanceId>,
    now: i64,
    deadline: Arc<AtomicI64>,
}

impl DirectSwitchoverPreparedActivityResolver {
    pub(crate) fn new(
        definition: &DirectSwitchoverDefinition,
        observations: &OperationObservations,
        addressed_instances: &BTreeMap<ReplicaId, ReplicaInstanceId>,
        now: i64,
        deadline: Arc<AtomicI64>,
    ) -> Self {
        Self {
            definition: definition.clone(),
            observations: observations.clone(),
            addressed_instances: addressed_instances.clone(),
            now,
            deadline,
        }
    }

    fn decode_logical(
        &self,
        logical: &ActivitySpec,
    ) -> Result<DirectActivity, PreparedActivityError> {
        let activity =
            DirectActivity::decode(logical).map_err(|_| PreparedActivityError::Encoding)?;
        if !activity.is_logical() {
            return Err(PreparedActivityError::Validation);
        }
        activity
            .validate(&self.definition)
            .map_err(|_| PreparedActivityError::Validation)?;
        self.deadline
            .store(activity.deadline_unix_seconds(), Ordering::Relaxed);
        Ok(activity)
    }
}

impl PreparedActivityResolver for DirectSwitchoverPreparedActivityResolver {
    fn resolve(
        &self,
        logical: &ActivitySpec,
        recorded: Option<&ActivitySpec>,
    ) -> Result<ActivitySpec, PreparedActivityError> {
        let logical_activity = self.decode_logical(logical)?;
        if let Some(recorded) = recorded {
            if recorded.name() != logical.name()
                || recorded.max_result_bytes() != logical.max_result_bytes()
            {
                return Ok(logical.clone());
            }
            let Ok(recorded_activity) = DirectActivity::decode(recorded) else {
                return Ok(logical.clone());
            };
            if recorded_activity.logical_predecessor() != logical_activity
                || recorded_activity.validate(&self.definition).is_err()
            {
                return Ok(logical.clone());
            }
            self.deadline
                .store(recorded_activity.deadline_unix_seconds(), Ordering::Relaxed);
            return Ok(recorded.clone());
        }
        logical_activity
            .prepare(
                &self.definition,
                &self.observations,
                &self.addressed_instances,
                self.now,
            )?
            .spec()
            .map_err(|_| PreparedActivityError::Encoding)
    }
}

pub struct DirectSwitchoverRunnerAdapter<'a> {
    initial: &'a crate::crd::DurableOperationStatus,
    definition: DirectSwitchoverDefinition,
    set: &'a KubericSet,
    current_pods: &'a [(ReplicaId, ReplicaInstanceId, &'a Pod)],
    api: &'a dyn ClusterApi,
    namespace: String,
    store: MeasuredDurableCheckpointStore,
    resolver: DirectSwitchoverPreparedActivityResolver,
    context: Option<SwitchoverRunnerContext>,
    now: i64,
    deadline: Arc<AtomicI64>,
}

impl<'a> DirectSwitchoverRunnerAdapter<'a> {
    pub fn new(
        initial: &'a crate::crd::DurableOperationStatus,
        set: &'a KubericSet,
        current_pods: &'a [(ReplicaId, ReplicaInstanceId, &'a Pod)],
        api: &'a dyn ClusterApi,
        store: MeasuredDurableCheckpointStore,
        now: i64,
    ) -> Result<Self, String> {
        let definition = DirectSwitchoverDefinition::from_initial(initial)?;
        let deadline = Arc::new(AtomicI64::new(definition.initial_deadline_unix_seconds));
        Ok(Self {
            initial,
            definition: definition.clone(),
            set,
            current_pods,
            api,
            namespace: set.metadata.namespace.clone().unwrap_or_default(),
            store,
            resolver: DirectSwitchoverPreparedActivityResolver::new(
                &definition,
                &OperationObservations::new(),
                &BTreeMap::new(),
                now,
                deadline.clone(),
            ),
            context: None,
            now,
            deadline,
        })
    }

    fn context(&self) -> Result<&SwitchoverRunnerContext, String> {
        self.context
            .as_ref()
            .ok_or_else(|| "direct switchover adapter was not prepared".to_string())
    }

    fn decode_activity(&self, activity: &LogicalActivityId) -> Result<DirectActivity, String> {
        let decoded = DirectActivity::decode(activity.spec())?;
        decoded.validate(&self.definition)?;
        self.deadline
            .store(decoded.deadline_unix_seconds(), Ordering::Relaxed);
        Ok(decoded)
    }

    fn observation(
        &self,
        activity: LogicalActivityId,
        contract: &DirectActivity,
        result: ExactBytes,
    ) -> DurableAdapterBoundary {
        match self.enrich_attestation(contract, result) {
            Ok(result) => {
                DurableAdapterBoundary::Observed(ActivityObservation::new(activity, result))
            }
            Err(error) => DurableAdapterBoundary::Isolated(error),
        }
    }

    fn enrich_attestation(
        &self,
        activity: &DirectActivity,
        result: ExactBytes,
    ) -> Result<ExactBytes, String> {
        let measurements = self.store.measurements();
        let accounting = measurements
            .completed_external_effect_count
            .zip(measurements.completed_passive_observation_count)
            .map(|(external, passive)| DirectActivityAccounting::new(external, passive));
        match activity {
            DirectActivity::AttestTargetTopology(_) => {
                let mut output = decode_activity_result::<AttestTargetTopologyActivity>(&result)
                    .map_err(|error| format!("decode target attestation result: {error}"))?;
                if let AttestTargetTopologyOutput::Attested {
                    accounting: value, ..
                } = &mut output
                {
                    *value = accounting;
                }
                encode_activity_result::<AttestTargetTopologyActivity>(&output)
                    .map_err(|error| format!("encode target attestation result: {error}"))
            }
            DirectActivity::AttestCompensatedTopology(_) => {
                let mut output = decode_activity_result::<AttestCompensatedTopologyActivity>(
                    &result,
                )
                .map_err(|error| format!("decode compensated attestation result: {error}"))?;
                if let AttestCompensatedTopologyOutput::Attested {
                    accounting: value, ..
                } = &mut output
                {
                    *value = accounting;
                }
                encode_activity_result::<AttestCompensatedTopologyActivity>(&output)
                    .map_err(|error| format!("encode compensated attestation result: {error}"))
            }
            _ => Ok(result),
        }
    }

    fn wait(&self, reason: &str, detail: &str) -> DurableAdapterBoundary {
        DurableAdapterBoundary::Wait {
            reason: reason.to_string(),
            detail: detail.to_string(),
        }
    }

    async fn evaluate_or_dispatch(
        &self,
        activity_id: LogicalActivityId,
        activity: &DirectActivity,
    ) -> DurableAdapterBoundary {
        let context = match self.context() {
            Ok(context) => context,
            Err(error) => return DurableAdapterBoundary::Isolated(error),
        };
        match activity.evaluate(&self.definition, &context.observations, self.now) {
            Ok(DirectEvaluation::Observe(result)) => {
                self.observation(activity_id, activity, result)
            }
            Ok(DirectEvaluation::AwaitEvidence) => self.wait(
                "AwaitingReplicaObservation",
                "direct switchover activity awaits exact authoritative evidence",
            ),
            Ok(DirectEvaluation::DispatchReplica { action, pending }) => {
                let Some(command) = activity.prepared_replica_command() else {
                    return DurableAdapterBoundary::Isolated(
                        "direct switchover resolver exposed an unprepared replica effect"
                            .to_string(),
                    );
                };
                if command.action_id != pending.action_id
                    || command.target_id != pending.target_id
                    || command.action_signature != action.signature()
                {
                    return DurableAdapterBoundary::Isolated(
                        "direct switchover prepared command does not match requested operation"
                            .to_string(),
                    );
                }
                let Some(handle) = context.handles.get(&command.target_id) else {
                    return self.wait(
                        "AwaitingReplicaObservation",
                        "direct switchover prepared command has no exact replica handle",
                    );
                };
                if handle.instance_id().as_str() != command.target_instance_id {
                    return self.wait(
                        "AwaitingReplicaObservation",
                        "direct switchover prepared command target incarnation changed",
                    );
                }
                match execute_replica_command(handle.as_ref(), command).await {
                    Ok(()) => self.wait(
                        "EffectExposed",
                        "direct switchover replica effect was exposed and awaits observation",
                    ),
                    Err(error) => self.dispatch_error(activity_id, activity, error),
                }
            }
            Ok(DirectEvaluation::DispatchLabel) => {
                let Some(command) = activity.prepared_label_command() else {
                    return DurableAdapterBoundary::Isolated(
                        "direct switchover resolver exposed an unprepared label effect".to_string(),
                    );
                };
                execute_label_command(self.api, &self.namespace, command).await;
                self.wait(
                    "EffectExposed",
                    "direct switchover UID-fenced label effect awaits exact observation",
                )
            }
            Err(error) => DurableAdapterBoundary::Isolated(error),
        }
    }

    fn dispatch_error(
        &self,
        activity_id: LogicalActivityId,
        activity: &DirectActivity,
        error: KubericError,
    ) -> DurableAdapterBoundary {
        let observation = match classify_dispatch_failure(&error) {
            DispatchFailureDisposition::ProvenNoAdmission => {
                super::activities::EffectObservation::ProvenNoAdmission {
                    observed_at_unix_seconds: self.now,
                }
            }
            DispatchFailureDisposition::DefiniteFailure
                if matches!(error, KubericError::RemoteAgentConflict(_)) =>
            {
                super::activities::EffectObservation::Conflicting {
                    observed_at_unix_seconds: self.now,
                    message: error.to_string(),
                }
            }
            DispatchFailureDisposition::DefiniteFailure => {
                super::activities::EffectObservation::Failed {
                    observed_at_unix_seconds: self.now,
                    message: error.to_string(),
                }
            }
            DispatchFailureDisposition::Unknown => {
                return self.wait(
                    "Quarantined",
                    "direct switchover replica dispatch outcome is unknown and awaits exact evidence",
                );
            }
        };
        match activity.encode_effect_observation(observation) {
            Ok(result)
                if matches!(
                    classify_dispatch_failure(&error),
                    DispatchFailureDisposition::ProvenNoAdmission
                ) =>
            {
                DurableAdapterBoundary::ObserveAndWait {
                    observation: Box::new(ActivityObservation::new(activity_id, result)),
                    reason: "RefreshingReplicaObservation".to_string(),
                    detail: "proven non-admission was persisted before the one allowed redelivery"
                        .to_string(),
                    requeue_after_seconds: 1,
                }
            }
            Ok(result) => self.observation(activity_id, activity, result),
            Err(error) => DurableAdapterBoundary::Isolated(error),
        }
    }
}

#[async_trait]
impl DurableOperationAdapter for DirectSwitchoverRunnerAdapter<'_> {
    type Resolver = DirectSwitchoverPreparedActivityResolver;
    type Terminal = DirectSwitchoverTerminalRecord;
    type Publication = DirectSwitchoverTerminalRecord;

    fn resolver(&self) -> &Self::Resolver {
        &self.resolver
    }

    async fn prepare(&mut self) -> Result<(), DurableAdapterBoundary> {
        let context =
            collect_switchover_runner_context(self.initial, self.set, self.api, self.current_pods)
                .await
                .map_err(DurableAdapterBoundary::Isolated)?;
        self.resolver = DirectSwitchoverPreparedActivityResolver::new(
            &self.definition,
            &context.observations,
            &context.addressed_instances,
            self.now,
            self.deadline.clone(),
        );
        self.context = Some(context);
        Ok(())
    }

    async fn observe_or_dispatch(
        &mut self,
        activity_id: &LogicalActivityId,
        attempt_id: kuberic_durable_execution::AttemptId,
        permit: &mut DurablePermitGuard,
    ) -> DurableAdapterBoundary {
        let activity = match self.decode_activity(activity_id) {
            Ok(activity) => activity,
            Err(error) => return DurableAdapterBoundary::Isolated(error),
        };
        let expected = match activity.spec() {
            Ok(expected) => expected,
            Err(error) => return DurableAdapterBoundary::Isolated(error),
        };
        if let Err(error) = permit.consume(&expected, activity_id, attempt_id, "switchover") {
            return DurableAdapterBoundary::Isolated(error);
        }
        self.evaluate_or_dispatch(activity_id.clone(), &activity)
            .await
    }

    async fn resolve_quarantine(
        &mut self,
        activity_id: LogicalActivityId,
        _attempt_id: kuberic_durable_execution::AttemptId,
    ) -> DurableAdapterBoundary {
        let activity = match self.decode_activity(&activity_id) {
            Ok(activity) => activity,
            Err(error) => return DurableAdapterBoundary::Isolated(error),
        };
        let observations = match self.context() {
            Ok(context) => &context.observations,
            Err(error) => return DurableAdapterBoundary::Isolated(error),
        };
        match resolve_direct_quarantine(&activity, &self.definition, observations, self.now) {
            Ok(DirectQuarantineOutcome::Observe(result)) => {
                self.observation(activity_id, &activity, result)
            }
            Ok(DirectQuarantineOutcome::AwaitEvidence) => self.wait(
                "Quarantined",
                "direct switchover exposed activity remains observation-only quarantined",
            ),
            Err(error) => DurableAdapterBoundary::Isolated(error),
        }
    }

    fn deadline_unix_seconds(&self) -> i64 {
        self.deadline.load(Ordering::Relaxed)
    }

    fn preparation_wait(&self, _error: &CheckpointError) -> DurableAdapterWait {
        DurableAdapterWait {
            reason: "AwaitingEffectPreparation".to_string(),
            detail: "direct switchover effect preparation awaits one exact observation snapshot"
                .to_string(),
            requeue_after_seconds: Some(1),
        }
    }

    fn checkpoint_disposition(&self, error: &CheckpointError) -> DurableCheckpointDisposition {
        if matches!(error, CheckpointError::UnsupportedFormat { .. }) {
            DurableCheckpointDisposition::Incompatible
        } else {
            DurableCheckpointDisposition::Rejected
        }
    }

    fn validate_terminal(
        &mut self,
        outcome: TerminalOutcome,
        completed_activity_count: u64,
    ) -> Result<Self::Terminal, DurableAdapterBoundary> {
        validate_direct_terminal(&self.definition, &outcome, completed_activity_count)
            .map_err(DurableAdapterBoundary::Rejected)
    }

    fn publication_handoff(&mut self, terminal: Self::Terminal) -> Self::Publication {
        terminal
    }
}

pub fn direct_execution_spec(
    execution_id: ExecutionId,
    initial_operation: crate::crd::DurableOperationStatus,
) -> Result<ExecutionSpec, String> {
    let definition = DirectSwitchoverDefinition::from_initial(&initial_operation)?;
    if definition.execution_id != initial_operation.execution_id {
        return Err("direct switchover admission execution identity changed".to_string());
    }
    let input = SwitchoverWorkflowInput {
        version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
        execution_id: encode_execution_id(execution_id),
        initial_operation,
    };
    let encoded = serde_json::to_vec(&input)
        .map_err(|error| format!("encode direct switchover workflow input: {error}"))?;
    validate_direct_workflow_input_bytes(encoded.len())?;
    Ok(ExecutionSpec::new(
        execution_id,
        ExactBytes::new(encoded),
        DIRECT_SWITCHOVER_MAX_TERMINAL_PAYLOAD_BYTES,
    ))
}

pub fn direct_checkpoint_limits() -> CheckpointLimits {
    CheckpointLimits::new(
        DIRECT_SWITCHOVER_MAX_ACTIVITY_RECORDS,
        DIRECT_SWITCHOVER_MAX_ACTIVE_ENCODED_BYTES,
        DIRECT_SWITCHOVER_MAX_TERMINAL_ENCODED_BYTES,
    )
    .expect("direct switchover checkpoint limits are nonzero")
}

pub fn direct_checkpoint_measurement_decoder() -> CheckpointMeasurementDecoder {
    CheckpointMeasurementDecoder::new(
        "switchover",
        classify_direct_checkpoint_activity,
        decode_direct_terminal_accounting,
    )
}

fn classify_direct_checkpoint_activity(spec: &ActivitySpec) -> Option<DurableActivityClass> {
    if !ALL_DIRECT_ACTIVITY_IDENTITIES
        .iter()
        .any(|(name, version)| *name == spec.name().name() && *version == spec.name().version())
    {
        return None;
    }
    let activity = DirectActivity::decode(spec).ok()?;
    Some(match activity {
        DirectActivity::Replica { request, .. } => {
            if request.prepared_command.is_some() {
                DurableActivityClass::ExternalEffect
            } else {
                DurableActivityClass::PassiveObservation
            }
        }
        DirectActivity::Label { request, .. } => {
            if request.prepared_command.is_some() {
                DurableActivityClass::ExternalEffect
            } else {
                DurableActivityClass::PassiveObservation
            }
        }
        DirectActivity::CaptureFrozenLsn(_)
        | DirectActivity::WaitTargetCaughtUp(_)
        | DirectActivity::AttestTargetTopology(_)
        | DirectActivity::AttestCompensatedTopology(_) => DurableActivityClass::PassiveObservation,
    })
}

fn decode_direct_terminal_accounting(
    outcome: &TerminalOutcome,
    completed_activity_count: u64,
) -> Option<DurableActivityAccounting> {
    let DirectSwitchoverTerminalRecord::Complete {
        snapshot,
        accounting: Some(accounting),
        ..
    } = serde_json::from_slice::<DirectSwitchoverTerminalRecord>(outcome.payload().as_slice())
        .ok()?
    else {
        return None;
    };
    if !matches!(outcome, TerminalOutcome::Succeeded(_))
        || !(2..=crate::crd::KUBERIC_MAX_REPLICAS as usize).contains(&snapshot.members.len())
        || accounting.total() != Some(completed_activity_count)
        || completed_activity_count > DIRECT_SWITCHOVER_MAX_ACTIVITY_RECORDS as u64
    {
        return None;
    }
    Some(DurableActivityAccounting {
        external_effect_count: accounting.external_effect_count,
        passive_observation_count: accounting.passive_observation_count,
    })
}

fn validate_direct_terminal(
    definition: &DirectSwitchoverDefinition,
    outcome: &TerminalOutcome,
    completed_activity_count: u64,
) -> Result<DirectSwitchoverTerminalRecord, String> {
    if completed_activity_count == 0
        || completed_activity_count > DIRECT_SWITCHOVER_MAX_ACTIVITY_RECORDS as u64
    {
        return Err("direct switchover terminal activity count is outside bounds".to_string());
    }
    let record =
        serde_json::from_slice::<DirectSwitchoverTerminalRecord>(outcome.payload().as_slice())
            .map_err(|error| format!("decode direct switchover terminal: {error}"))?;
    match (&outcome, &record) {
        (
            TerminalOutcome::Succeeded(_),
            DirectSwitchoverTerminalRecord::Complete {
                snapshot,
                compensated: false,
                reason: None,
                accounting: Some(accounting),
            },
        ) if same_topology(snapshot, &definition.target_snapshot)
            && accounting.total() == Some(completed_activity_count) => {}
        (
            TerminalOutcome::Succeeded(_),
            DirectSwitchoverTerminalRecord::Complete {
                snapshot,
                compensated: true,
                reason: Some(_),
                accounting: Some(accounting),
            },
        ) if is_valid_compensation_topology(snapshot, definition)
            && accounting.total() == Some(completed_activity_count) => {}
        (TerminalOutcome::Failed(_), DirectSwitchoverTerminalRecord::Stopped { .. }) => {}
        _ => {
            return Err(
                "direct switchover terminal kind or topology conflicts with admission".to_string(),
            );
        }
    }
    Ok(record)
}

pub fn validate_direct_workflow_input_bytes(actual: usize) -> Result<(), String> {
    validate_maximum(
        "workflow input",
        actual,
        DIRECT_SWITCHOVER_MAX_WORKFLOW_INPUT_BYTES,
    )
}

#[cfg(test)]
pub fn validate_direct_active_checkpoint_bytes(actual: usize) -> Result<(), String> {
    validate_maximum(
        "active checkpoint",
        actual,
        DIRECT_SWITCHOVER_MAX_ACTIVE_ENCODED_BYTES,
    )
}

#[cfg(test)]
pub fn validate_direct_terminal_checkpoint_bytes(actual: usize) -> Result<(), String> {
    validate_maximum(
        "terminal checkpoint",
        actual,
        DIRECT_SWITCHOVER_MAX_TERMINAL_ENCODED_BYTES,
    )
}

#[cfg(test)]
pub fn validate_direct_terminal_payload_bytes(actual: usize) -> Result<(), String> {
    validate_maximum(
        "terminal payload",
        actual,
        DIRECT_SWITCHOVER_MAX_TERMINAL_PAYLOAD_BYTES as usize,
    )
}

#[cfg(test)]
pub fn validate_direct_transition_fuel(actual: usize) -> Result<(), String> {
    validate_maximum(
        "transition fuel",
        actual,
        DIRECT_SWITCHOVER_MAX_TRANSITION_FUEL,
    )
}

pub fn validate_direct_runner_fuel(actual: usize) -> Result<(), String> {
    if actual == 0 {
        return Err("direct switchover runner fuel must be positive".to_string());
    }
    validate_maximum("runner fuel", actual, DIRECT_SWITCHOVER_MAX_RUNNER_FUEL)
}

#[cfg(test)]
pub fn validate_direct_error_bytes(actual: usize) -> Result<(), String> {
    validate_maximum("error", actual, DIRECT_SWITCHOVER_MAX_ERROR_BYTES)
}

fn validate_maximum(label: &str, actual: usize, maximum: usize) -> Result<(), String> {
    if actual > maximum {
        Err(format!(
            "direct switchover {label} is {actual}; maximum is {maximum}"
        ))
    } else {
        Ok(())
    }
}

#[cfg(test)]
pub fn direct_execution_contract(execution: ExecutionSpec) -> ExecutionContract {
    ExecutionContract::with_encoded_limits(
        execution,
        DIRECT_SWITCHOVER_MAX_ACTIVE_ENCODED_BYTES as u64,
        DIRECT_SWITCHOVER_MAX_TERMINAL_ENCODED_BYTES as u64,
    )
}

#[cfg(test)]
mod tests {
    use std::{
        collections::{BTreeMap, BTreeSet},
        sync::{Arc, Mutex as StdMutex},
    };

    use async_trait::async_trait;
    use k8s_openapi::{
        api::core::v1::{PersistentVolumeClaim, Pod, Service},
        apimachinery::pkg::apis::meta::v1::ObjectMeta,
    };
    use kuberic_core::{
        driver::ReplicaHandle,
        error::KubericError,
        types::{
            AccessStatus, AgentControlVersion, AgentGeneration, CorrelatedActionObservation,
            CorrelatedControlActionAcknowledgement, CorrelatedControlActionRequest,
            DurableActionErrorClass, DurableActionObservation, DurableActionState,
            DurableReplicaAction, Epoch, ReplicaAgentStatus, ReplicaConfigurationMemberStatus,
            ReplicaConfigurationMode, ReplicaConfigurationStatus, ReplicaId, ReplicaInstanceId,
            ReplicaStatusInfo, Role,
        },
    };
    use kuberic_durable_execution::{
        ActivityName, ActivityRecord, ActivitySequence, CheckpointEnvelope, CheckpointError,
        CheckpointPayload, CheckpointStore, DurableActivity, DurableHost, HostEpoch, HostOutcome,
        InMemoryCheckpointStore, InMemoryFault, StoreErrorKind,
    };

    use crate::{
        cluster_api::ClusterApi,
        crd::{
            EpochStatus, KubericSet, KubericSetSpec, KubericSetStatus, PvcRetentionPolicy,
            StablePartitionSnapshotStatus, StableReplicaRoleStatus, StableReplicaSnapshotStatus,
        },
        durable::{
            checkpoint_store::{
                DurableCheckpointMeasurementsSnapshot, DurableCheckpointStore,
                MeasuredDurableCheckpointStore,
            },
            runner::{DurableRunner, DurableRunnerOutcome},
            switchover_execution::direct_initial_operation,
        },
    };

    use super::super::{
        activities::{
            AttestCompensatedTopologyActivity, AttestTargetTopologyActivity,
            CaptureFrozenLsnActivity, CompensateDistributeReplicaEpochActivity,
            CompensatePromoteOldPrimaryActivity, DemoteOldPrimaryActivity,
            DistributeReplicaEpochActivity, InstallCompensationCatchUpConfigurationActivity,
            InstallCompensationCurrentConfigurationActivity,
            InstallTargetCatchUpConfigurationActivity, InstallTargetCurrentConfigurationActivity,
            LabelOperationRequest, PromoteTargetActivity, PublishOldPrimarySecondaryLabelActivity,
            PublishTargetPrimaryLabelActivity, ReplicaOperationRequest,
            RestoreOldPrimaryLabelActivity, RestorePreviousCurrentConfigurationActivity,
            RestoreTargetSecondaryLabelActivity, RevokeWritesActivity, WaitTargetCaughtUpActivity,
            WaitTargetWriteQuorumActivity,
        },
        prepare::{DirectLabelOperation, DirectReplicaOperation},
    };
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Scenario {
        Success,
        PrePromotionCompensation,
        PostPromotionCompensation,
    }

    fn expected_scenario_activity_identities(
        member_count: usize,
        scenario: Scenario,
        redeliver_replica_effects: bool,
    ) -> Vec<(String, u32)> {
        fn push_replica<A: DurableActivity>(identities: &mut Vec<(String, u32)>, redeliver: bool) {
            identities.push((A::NAME.to_string(), A::VERSION));
            if redeliver {
                identities.push((A::NAME.to_string(), A::VERSION));
            }
        }

        fn push_once<A: DurableActivity>(identities: &mut Vec<(String, u32)>) {
            identities.push((A::NAME.to_string(), A::VERSION));
        }

        let mut identities = Vec::new();
        push_replica::<RevokeWritesActivity>(&mut identities, redeliver_replica_effects);
        push_once::<CaptureFrozenLsnActivity>(&mut identities);
        push_once::<WaitTargetCaughtUpActivity>(&mut identities);
        match scenario {
            Scenario::PrePromotionCompensation => {
                push_replica::<RestorePreviousCurrentConfigurationActivity>(
                    &mut identities,
                    redeliver_replica_effects,
                );
                push_once::<AttestCompensatedTopologyActivity>(&mut identities);
            }
            Scenario::Success => {
                push_replica::<DemoteOldPrimaryActivity>(
                    &mut identities,
                    redeliver_replica_effects,
                );
                push_replica::<PromoteTargetActivity>(&mut identities, redeliver_replica_effects);
                for _ in 0..member_count.saturating_sub(2) {
                    push_replica::<DistributeReplicaEpochActivity>(
                        &mut identities,
                        redeliver_replica_effects,
                    );
                }
                push_replica::<InstallTargetCatchUpConfigurationActivity>(
                    &mut identities,
                    redeliver_replica_effects,
                );
                push_replica::<WaitTargetWriteQuorumActivity>(
                    &mut identities,
                    redeliver_replica_effects,
                );
                push_replica::<InstallTargetCurrentConfigurationActivity>(
                    &mut identities,
                    redeliver_replica_effects,
                );
                push_once::<PublishTargetPrimaryLabelActivity>(&mut identities);
                push_once::<PublishOldPrimarySecondaryLabelActivity>(&mut identities);
                push_once::<AttestTargetTopologyActivity>(&mut identities);
            }
            Scenario::PostPromotionCompensation => {
                push_replica::<DemoteOldPrimaryActivity>(
                    &mut identities,
                    redeliver_replica_effects,
                );
                push_replica::<PromoteTargetActivity>(&mut identities, redeliver_replica_effects);
                push_replica::<CompensatePromoteOldPrimaryActivity>(
                    &mut identities,
                    redeliver_replica_effects,
                );
                for _ in 0..member_count.saturating_sub(1) {
                    push_replica::<CompensateDistributeReplicaEpochActivity>(
                        &mut identities,
                        redeliver_replica_effects,
                    );
                }
                push_replica::<InstallCompensationCatchUpConfigurationActivity>(
                    &mut identities,
                    redeliver_replica_effects,
                );
                push_replica::<InstallCompensationCurrentConfigurationActivity>(
                    &mut identities,
                    redeliver_replica_effects,
                );
                push_once::<RestoreOldPrimaryLabelActivity>(&mut identities);
                push_once::<RestoreTargetSecondaryLabelActivity>(&mut identities);
                push_once::<AttestCompensatedTopologyActivity>(&mut identities);
            }
        }
        identities
    }

    fn expected_persisted_scenario_activity_prefix(
        member_count: usize,
        scenario: Scenario,
        redeliver_replica_effects: bool,
    ) -> Vec<(String, u32)> {
        let mut identities = expected_scenario_activity_identities(
            member_count,
            scenario,
            redeliver_replica_effects,
        );
        let terminal_fused_suffix = match scenario {
            Scenario::Success | Scenario::PrePromotionCompensation => 1,
            Scenario::PostPromotionCompensation => 3,
        };
        identities.truncate(identities.len() - terminal_fused_suffix);
        identities
    }

    #[derive(Default)]
    struct TestWorld {
        statuses: BTreeMap<ReplicaId, ReplicaStatusInfo>,
        labels: BTreeMap<ReplicaId, String>,
        attempts: BTreeMap<String, usize>,
        requests: Vec<CorrelatedControlActionRequest>,
        label_patches: Vec<(ReplicaId, String)>,
        fail_sequences: BTreeSet<u32>,
        busy_first_for_every_action: bool,
        busy_always_sequences: BTreeSet<u32>,
        unknown_after_apply_sequences: BTreeSet<u32>,
        unknown_without_apply_sequences: BTreeSet<u32>,
    }

    impl TestWorld {
        fn pods(&self) -> Vec<Pod> {
            self.statuses
                .iter()
                .map(|(id, status)| Pod {
                    metadata: ObjectMeta {
                        name: Some(format!("set-{id}")),
                        namespace: Some("default".to_string()),
                        uid: Some(status.instance_id.to_string()),
                        labels: Some(BTreeMap::from([
                            ("kuberic.io/replica-id".to_string(), id.to_string()),
                            (
                                "kuberic.io/role".to_string(),
                                self.labels
                                    .get(id)
                                    .cloned()
                                    .unwrap_or_else(|| "secondary".to_string()),
                            ),
                        ])),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .collect()
        }

        fn change_generation(&mut self, target_id: ReplicaId) {
            let status = self.statuses.get_mut(&target_id).unwrap();
            status.agent.generation =
                AgentGeneration::parse("ffffffffffffffffffffffffffffffff").unwrap();
            status.agent.control_version = AgentControlVersion::new(1);
            status.agent.current_action = None;
            status.agent.retained_terminal_actions.clear();
        }
    }

    struct TestHandle {
        id: ReplicaId,
        world: Arc<StdMutex<TestWorld>>,
    }

    #[async_trait]
    impl ReplicaHandle for TestHandle {
        fn id(&self) -> ReplicaId {
            self.id
        }

        fn instance_id(&self) -> ReplicaInstanceId {
            self.world
                .lock()
                .unwrap()
                .statuses
                .get(&self.id)
                .unwrap()
                .instance_id
                .clone()
        }

        fn current_progress(&self) -> i64 {
            self.world
                .lock()
                .unwrap()
                .statuses
                .get(&self.id)
                .unwrap()
                .current_progress
        }

        fn catch_up_capability(&self) -> i64 {
            self.world
                .lock()
                .unwrap()
                .statuses
                .get(&self.id)
                .unwrap()
                .catch_up_capability
                .unwrap_or_default()
        }

        fn control_address(&self) -> String {
            format!("http://set-{}:9090", self.id)
        }

        fn replicator_address(&self) -> String {
            format!("http://set-{}:9091", self.id)
        }

        async fn get_status(&self) -> kuberic_core::Result<ReplicaStatusInfo> {
            Ok(self
                .world
                .lock()
                .unwrap()
                .statuses
                .get(&self.id)
                .unwrap()
                .clone())
        }

        async fn execute_correlated_control_action(
            &self,
            request: CorrelatedControlActionRequest,
        ) -> kuberic_core::Result<CorrelatedControlActionAcknowledgement> {
            let mut world = self.world.lock().unwrap();
            let sequence = action_sequence(&request.action_id);
            let attempt = {
                let attempt = world.attempts.entry(request.action_id.clone()).or_default();
                *attempt += 1;
                *attempt
            };
            world.requests.push(request.clone());
            if world.busy_always_sequences.contains(&sequence)
                || (world.busy_first_for_every_action && attempt == 1)
            {
                return Err(KubericError::AgentBusy);
            }
            if world.fail_sequences.contains(&sequence) {
                return Err(KubericError::RemoteAgentTerminalFailure {
                    class: DurableActionErrorClass::Internal,
                    message: format!("injected failure for sequence {sequence}"),
                });
            }
            if world.unknown_without_apply_sequences.remove(&sequence) {
                return Err(KubericError::Internal(Box::new(std::io::Error::other(
                    "ambiguous dispatch before application",
                ))));
            }
            let acknowledgement = apply_action(&mut world, &request)?;
            if world.unknown_after_apply_sequences.remove(&sequence) {
                return Err(KubericError::Internal(Box::new(std::io::Error::other(
                    "lost reply after application",
                ))));
            }
            Ok(acknowledgement)
        }
    }

    struct TestApi {
        world: Arc<StdMutex<TestWorld>>,
        apply_labels: bool,
    }

    #[async_trait]
    impl ClusterApi for TestApi {
        async fn list_pods(&self, _: &str, _: &str) -> Result<Vec<Pod>, String> {
            Ok(self.world.lock().unwrap().pods())
        }

        async fn create_pod(&self, _: &str, _: &Pod) -> Result<(), String> {
            unreachable!()
        }

        async fn delete_pod(&self, _: &str, _: &str, _: &str) -> Result<(), String> {
            unreachable!()
        }

        async fn patch_pod_labels(
            &self,
            _: &str,
            _: &str,
            _: BTreeMap<String, String>,
        ) -> Result<(), String> {
            unreachable!()
        }

        async fn patch_pod_labels_if_uid(
            &self,
            _: &str,
            pod_name: &str,
            expected_uid: &str,
            labels: BTreeMap<String, String>,
        ) -> Result<(), String> {
            let mut world = self.world.lock().unwrap();
            let Some((&id, _)) = world.statuses.iter().find(|(id, status)| {
                pod_name == format!("set-{id}") && status.instance_id.as_str() == expected_uid
            }) else {
                return Err("UID-fenced test label target was not found".to_string());
            };
            let role = labels
                .get("kuberic.io/role")
                .cloned()
                .ok_or_else(|| "test label patch has no role".to_string())?;
            world.label_patches.push((id, role.clone()));
            if self.apply_labels {
                world.labels.insert(id, role);
            }
            Ok(())
        }

        async fn patch_set_status(
            &self,
            _: &str,
            _: &str,
            _: &KubericSetStatus,
            _: Option<&str>,
        ) -> Result<(), String> {
            unreachable!()
        }

        async fn create_replica_handle(
            &self,
            replica_id: ReplicaId,
            _: &Pod,
            _: &KubericSetSpec,
        ) -> Result<Box<dyn ReplicaHandle>, String> {
            Ok(Box::new(TestHandle {
                id: replica_id,
                world: self.world.clone(),
            }))
        }

        async fn get_pvc(&self, _: &str, _: &str) -> Result<PersistentVolumeClaim, String> {
            unreachable!()
        }

        async fn create_pvc(&self, _: &str, _: &PersistentVolumeClaim) -> Result<(), String> {
            unreachable!()
        }

        async fn list_pvcs(&self, _: &str, _: &str) -> Result<Vec<PersistentVolumeClaim>, String> {
            unreachable!()
        }

        async fn delete_pvc(&self, _: &str, _: &str) -> Result<(), String> {
            unreachable!()
        }

        async fn get_service(&self, _: &str, _: &str) -> Result<Service, String> {
            unreachable!()
        }

        async fn create_service(&self, _: &str, _: &Service) -> Result<(), String> {
            unreachable!()
        }

        async fn delete_service(&self, _: &str, _: &str) -> Result<(), String> {
            unreachable!()
        }
    }

    struct ScenarioResult {
        terminal: DirectSwitchoverTerminalRecord,
        measurements: DurableCheckpointMeasurementsSnapshot,
        requests: usize,
        label_patches: usize,
        activity_identities: Vec<(String, u32)>,
    }

    async fn record_scenario_activity_identities(
        backend: &InMemoryCheckpointStore,
        execution: &ExecutionSpec,
        longest: &mut Vec<(String, u32)>,
    ) {
        let Some(stored) = backend.load(execution.execution_id()).await.unwrap() else {
            return;
        };
        let payload = stored
            .checkpoint()
            .decode_and_validate(execution, direct_checkpoint_limits())
            .unwrap();
        let Some(activities) = payload.active_activities() else {
            return;
        };
        let identities = activities
            .iter()
            .map(|activity| {
                (
                    activity.name().name().to_string(),
                    activity.name().version(),
                )
            })
            .collect::<Vec<_>>();
        let shared = longest.len().min(identities.len());
        assert_eq!(identities[..shared], longest[..shared]);
        if identities.len() > longest.len() {
            *longest = identities;
        }
    }

    async fn run_scenario(
        member_count: usize,
        scenario: Scenario,
        busy_first_for_every_action: bool,
    ) -> ScenarioResult {
        let initial = direct_initial_operation(
            &format!("set-{member_count}-{scenario:?}"),
            snapshot(member_count),
            2,
            100,
        )
        .unwrap();
        let execution_id = ExecutionId::from_bytes([u8::try_from(member_count).unwrap(); 16]);
        let execution = direct_execution_spec(execution_id, initial.clone()).unwrap();
        let world = Arc::new(StdMutex::new(world_for(&initial, scenario)));
        world.lock().unwrap().busy_first_for_every_action = busy_first_for_every_action;
        let api = TestApi {
            world: world.clone(),
            apply_labels: true,
        };
        let set = test_set(member_count);
        let backend = InMemoryCheckpointStore::new();
        let store = MeasuredDurableCheckpointStore::with_decoder(
            execution_id,
            DurableCheckpointStore::InMemory(backend.clone()),
            direct_checkpoint_measurement_decoder(),
        );
        let mut host = DurableHost::new(
            store,
            HostEpoch::from_bytes([91; 16]),
            direct_checkpoint_limits(),
        );
        let runner = DurableRunner::new(DIRECT_SWITCHOVER_MAX_RUNNER_FUEL).unwrap();
        let mut now = 100;
        let mut activity_identities = Vec::new();

        for _ in 0..512 {
            let pods = world.lock().unwrap().pods();
            let current_pods = pod_references(&pods);
            let mut adapter = DirectSwitchoverRunnerAdapter::new(
                &initial,
                &set,
                &current_pods,
                &api,
                host.store().clone(),
                now,
            )
            .unwrap();
            let outcome = runner
                .run(
                    &mut host,
                    &DirectSwitchoverWorkflow,
                    execution.clone(),
                    &mut adapter,
                    now,
                )
                .await;
            record_scenario_activity_identities(&backend, &execution, &mut activity_identities)
                .await;
            match outcome {
                DurableRunnerOutcome::Terminal(terminal) => {
                    let world = world.lock().unwrap();
                    return ScenarioResult {
                        terminal,
                        measurements: host.store().measurements(),
                        requests: world.requests.len(),
                        label_patches: world.label_patches.len(),
                        activity_identities,
                    };
                }
                DurableRunnerOutcome::Active { .. }
                | DurableRunnerOutcome::ReloadRequired { .. } => {
                    now += 1;
                }
                other => panic!("unexpected direct switchover runner outcome: {other:?}"),
            }
        }
        panic!("direct switchover scenario did not terminate");
    }

    #[tokio::test]
    async fn direct_switchover_adapter_runs_success_and_both_compensation_families_at_scale() {
        for member_count in [2, 4, 9] {
            for scenario in [
                Scenario::Success,
                Scenario::PrePromotionCompensation,
                Scenario::PostPromotionCompensation,
            ] {
                let result = run_scenario(member_count, scenario, false).await;
                match (&result.terminal, scenario) {
                    (
                        DirectSwitchoverTerminalRecord::Complete {
                            compensated: false, ..
                        },
                        Scenario::Success,
                    ) => {}
                    (
                        DirectSwitchoverTerminalRecord::Complete {
                            compensated: true, ..
                        },
                        Scenario::PrePromotionCompensation | Scenario::PostPromotionCompensation,
                    ) => {}
                    other => panic!("unexpected direct terminal: {other:?}"),
                }
                assert_eq!(
                    result.measurements.completed_passive_observation_count,
                    Some(if scenario == Scenario::PostPromotionCompensation {
                        5
                    } else {
                        3
                    })
                );
                assert!(
                    result
                        .measurements
                        .completed_activity_count
                        .is_some_and(|count| count <= DIRECT_SWITCHOVER_MAX_ACTIVITY_RECORDS as u64)
                );
                let (expected_activities, expected_external) = match scenario {
                    Scenario::Success => (member_count + 9, member_count + 6),
                    Scenario::PrePromotionCompensation => (5, 2),
                    Scenario::PostPromotionCompensation => (member_count + 10, member_count + 5),
                };
                assert_eq!(
                    result.measurements.completed_activity_count,
                    Some(expected_activities as u64)
                );
                assert_eq!(
                    result.measurements.completed_external_effect_count,
                    Some(expected_external as u64)
                );
                assert_eq!(
                    result.activity_identities,
                    expected_persisted_scenario_activity_prefix(member_count, scenario, false)
                );
                assert_eq!(
                    result.measurements.accepted_writes,
                    expected_activities as u64 + 1
                );
                assert!(result.requests > 0);
                if scenario == Scenario::Success {
                    assert_eq!(result.label_patches, 2);
                } else {
                    assert_eq!(result.label_patches, 0);
                }
            }
        }
    }

    #[tokio::test]
    async fn direct_switchover_nine_member_success_all_replica_redelivery_is_measured() {
        let result = run_scenario(9, Scenario::Success, true).await;
        assert_eq!(result.measurements.completed_activity_count, Some(31));
        assert_eq!(
            result.measurements.completed_external_effect_count,
            Some(28)
        );
        assert_eq!(
            result.measurements.completed_passive_observation_count,
            Some(3)
        );
        assert_eq!(result.requests, 26);
        assert_eq!(result.label_patches, 2);
        assert_eq!(result.measurements.accepted_writes, 45);
        assert_eq!(
            result.activity_identities,
            expected_persisted_scenario_activity_prefix(9, Scenario::Success, true)
        );
    }

    #[tokio::test]
    async fn direct_switchover_nine_member_max_fault_measurement_fits_exact_limits() {
        let result = run_scenario(9, Scenario::PostPromotionCompensation, true).await;
        assert_eq!(
            result.measurements.completed_activity_count,
            Some(DIRECT_SWITCHOVER_MAX_ACTIVITY_RECORDS as u64)
        );
        assert_eq!(
            result.measurements.completed_external_effect_count,
            Some(28)
        );
        assert_eq!(
            result.measurements.completed_passive_observation_count,
            Some(5)
        );
        assert!(
            result.measurements.maximum_active_checkpoint_bytes
                <= DIRECT_SWITCHOVER_MAX_ACTIVE_ENCODED_BYTES
        );
        assert!(
            result.measurements.maximum_terminal_checkpoint_bytes
                <= DIRECT_SWITCHOVER_MAX_TERMINAL_ENCODED_BYTES
        );
        assert_eq!(result.requests, 28);
        assert_eq!(result.label_patches, 0);
        assert_eq!(result.measurements.accepted_writes, 48);
        assert_eq!(
            result.activity_identities,
            expected_persisted_scenario_activity_prefix(
                9,
                Scenario::PostPromotionCompensation,
                true,
            )
        );
        assert_eq!(
            result.activity_identities.len(),
            DIRECT_SWITCHOVER_MAX_ACTIVITY_RECORDS - 3
        );
        eprintln!(
            "direct switchover max-fault measurement: records={}, active={} (headroom={}), terminal={} (headroom={})",
            result.measurements.completed_activity_count.unwrap(),
            result.measurements.maximum_active_checkpoint_bytes,
            DIRECT_SWITCHOVER_MAX_ACTIVE_ENCODED_BYTES
                - result.measurements.maximum_active_checkpoint_bytes,
            result.measurements.maximum_terminal_checkpoint_bytes,
            DIRECT_SWITCHOVER_MAX_TERMINAL_ENCODED_BYTES
                - result.measurements.maximum_terminal_checkpoint_bytes,
        );
    }

    #[tokio::test]
    async fn direct_switchover_adapter_is_deterministic_for_one_hundred_runs() {
        let baseline = run_scenario(2, Scenario::Success, false).await;
        for _ in 0..100 {
            let replay = run_scenario(2, Scenario::Success, false).await;
            assert_eq!(replay.terminal, baseline.terminal);
            assert_eq!(replay.measurements, baseline.measurements);
            assert_eq!(replay.requests, baseline.requests);
            assert_eq!(replay.label_patches, baseline.label_patches);
            assert_eq!(replay.activity_identities, baseline.activity_identities);
        }
    }

    #[tokio::test]
    async fn direct_switchover_lost_reply_resolves_without_duplicate_effect() {
        let initial = direct_initial_operation("lost-reply", snapshot(2), 2, 100).unwrap();
        let execution_id = ExecutionId::from_bytes([61; 16]);
        let execution = direct_execution_spec(execution_id, initial.clone()).unwrap();
        let mut world_value = world_for(&initial, Scenario::Success);
        world_value.unknown_after_apply_sequences.insert(1);
        let world = Arc::new(StdMutex::new(world_value));
        let api = TestApi {
            world: world.clone(),
            apply_labels: true,
        };
        let set = test_set(2);
        let mut host = DurableHost::new(
            MeasuredDurableCheckpointStore::with_decoder(
                execution_id,
                DurableCheckpointStore::InMemory(InMemoryCheckpointStore::new()),
                direct_checkpoint_measurement_decoder(),
            ),
            HostEpoch::from_bytes([62; 16]),
            direct_checkpoint_limits(),
        );
        let runner = DurableRunner::new(DIRECT_SWITCHOVER_MAX_RUNNER_FUEL).unwrap();
        let mut now = 100;
        for _ in 0..64 {
            let pods = world.lock().unwrap().pods();
            let current_pods = pod_references(&pods);
            let mut adapter = DirectSwitchoverRunnerAdapter::new(
                &initial,
                &set,
                &current_pods,
                &api,
                host.store().clone(),
                now,
            )
            .unwrap();
            match runner
                .run(
                    &mut host,
                    &DirectSwitchoverWorkflow,
                    execution.clone(),
                    &mut adapter,
                    now,
                )
                .await
            {
                DurableRunnerOutcome::Terminal(_) => break,
                DurableRunnerOutcome::Active { .. }
                | DurableRunnerOutcome::ReloadRequired { .. } => now += 1,
                other => panic!("unexpected lost-reply outcome: {other:?}"),
            }
        }
        let world = world.lock().unwrap();
        assert_eq!(
            world
                .requests
                .iter()
                .filter(|request| action_sequence(&request.action_id) == 1)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn direct_switchover_unknown_completion_write_reloads_without_duplicate_effect() {
        let initial = direct_initial_operation("unknown-completion", snapshot(2), 2, 100).unwrap();
        let execution_id = ExecutionId::from_bytes([71; 16]);
        let execution = direct_execution_spec(execution_id, initial.clone()).unwrap();
        let world = Arc::new(StdMutex::new(world_for(&initial, Scenario::Success)));
        let api = TestApi {
            world: world.clone(),
            apply_labels: true,
        };
        let set = test_set(2);
        let backend = InMemoryCheckpointStore::new();
        let mut host = DurableHost::new(
            MeasuredDurableCheckpointStore::with_decoder(
                execution_id,
                DurableCheckpointStore::InMemory(backend.clone()),
                direct_checkpoint_measurement_decoder(),
            ),
            HostEpoch::from_bytes([72; 16]),
            direct_checkpoint_limits(),
        );
        let runner = DurableRunner::new(DIRECT_SWITCHOVER_MAX_RUNNER_FUEL).unwrap();

        let pods = world.lock().unwrap().pods();
        let current_pods = pod_references(&pods);
        let mut adapter = DirectSwitchoverRunnerAdapter::new(
            &initial,
            &set,
            &current_pods,
            &api,
            host.store().clone(),
            100,
        )
        .unwrap();
        assert!(matches!(
            runner
                .run(
                    &mut host,
                    &DirectSwitchoverWorkflow,
                    execution.clone(),
                    &mut adapter,
                    100,
                )
                .await,
            DurableRunnerOutcome::Active { .. }
        ));

        backend.fail_next_compare_and_swap(InMemoryFault::OutcomeUnknownAfterApply);
        let pods = world.lock().unwrap().pods();
        let current_pods = pod_references(&pods);
        let mut adapter = DirectSwitchoverRunnerAdapter::new(
            &initial,
            &set,
            &current_pods,
            &api,
            host.store().clone(),
            101,
        )
        .unwrap();
        assert!(matches!(
            runner
                .run(
                    &mut host,
                    &DirectSwitchoverWorkflow,
                    execution.clone(),
                    &mut adapter,
                    101,
                )
                .await,
            DurableRunnerOutcome::ReloadRequired { .. }
        ));

        let mut now = 102;
        let mut completed = false;
        for _ in 0..64 {
            let pods = world.lock().unwrap().pods();
            let current_pods = pod_references(&pods);
            let mut adapter = DirectSwitchoverRunnerAdapter::new(
                &initial,
                &set,
                &current_pods,
                &api,
                host.store().clone(),
                now,
            )
            .unwrap();
            match runner
                .run(
                    &mut host,
                    &DirectSwitchoverWorkflow,
                    execution.clone(),
                    &mut adapter,
                    now,
                )
                .await
            {
                DurableRunnerOutcome::Terminal(_) => {
                    completed = true;
                    break;
                }
                DurableRunnerOutcome::Active { .. }
                | DurableRunnerOutcome::ReloadRequired { .. } => now += 1,
                other => panic!("unexpected unknown-completion outcome: {other:?}"),
            }
        }
        assert!(completed);
        let world = world.lock().unwrap();
        assert_eq!(
            world
                .requests
                .iter()
                .filter(|request| action_sequence(&request.action_id) == 1)
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn direct_switchover_generation_change_allows_only_one_same_action_redelivery() {
        let initial = direct_initial_operation("generation-change", snapshot(2), 2, 100).unwrap();
        let execution_id = ExecutionId::from_bytes([63; 16]);
        let execution = direct_execution_spec(execution_id, initial.clone()).unwrap();
        let mut world_value = world_for(&initial, Scenario::Success);
        world_value.unknown_without_apply_sequences.insert(1);
        let world = Arc::new(StdMutex::new(world_value));
        let api = TestApi {
            world: world.clone(),
            apply_labels: true,
        };
        let set = test_set(2);
        let mut host = DurableHost::new(
            MeasuredDurableCheckpointStore::with_decoder(
                execution_id,
                DurableCheckpointStore::InMemory(InMemoryCheckpointStore::new()),
                direct_checkpoint_measurement_decoder(),
            ),
            HostEpoch::from_bytes([64; 16]),
            direct_checkpoint_limits(),
        );
        let runner = DurableRunner::new(DIRECT_SWITCHOVER_MAX_RUNNER_FUEL).unwrap();

        let pods = world.lock().unwrap().pods();
        let current_pods = pod_references(&pods);
        let mut adapter = DirectSwitchoverRunnerAdapter::new(
            &initial,
            &set,
            &current_pods,
            &api,
            host.store().clone(),
            100,
        )
        .unwrap();
        assert!(matches!(
            runner
                .run(
                    &mut host,
                    &DirectSwitchoverWorkflow,
                    execution.clone(),
                    &mut adapter,
                    100,
                )
                .await,
            DurableRunnerOutcome::Active { .. }
        ));
        world.lock().unwrap().change_generation(1);

        let mut now = 101;
        for _ in 0..64 {
            let pods = world.lock().unwrap().pods();
            let current_pods = pod_references(&pods);
            let mut adapter = DirectSwitchoverRunnerAdapter::new(
                &initial,
                &set,
                &current_pods,
                &api,
                host.store().clone(),
                now,
            )
            .unwrap();
            match runner
                .run(
                    &mut host,
                    &DirectSwitchoverWorkflow,
                    execution.clone(),
                    &mut adapter,
                    now,
                )
                .await
            {
                DurableRunnerOutcome::Terminal(_) => break,
                DurableRunnerOutcome::Active { .. }
                | DurableRunnerOutcome::ReloadRequired { .. } => now += 1,
                other => panic!("unexpected generation-change outcome: {other:?}"),
            }
        }
        let world = world.lock().unwrap();
        assert_eq!(
            world
                .requests
                .iter()
                .filter(|request| action_sequence(&request.action_id) == 1)
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn direct_switchover_second_proven_non_admission_stops_without_third_dispatch() {
        let initial = direct_initial_operation("bounded-redelivery", snapshot(2), 2, 100).unwrap();
        let execution_id = ExecutionId::from_bytes([74; 16]);
        let execution = direct_execution_spec(execution_id, initial.clone()).unwrap();
        let mut world_value = world_for(&initial, Scenario::Success);
        world_value.busy_always_sequences.insert(1);
        let world = Arc::new(StdMutex::new(world_value));
        let api = TestApi {
            world: world.clone(),
            apply_labels: true,
        };
        let set = test_set(2);
        let mut host = DurableHost::new(
            MeasuredDurableCheckpointStore::with_decoder(
                execution_id,
                DurableCheckpointStore::InMemory(InMemoryCheckpointStore::new()),
                direct_checkpoint_measurement_decoder(),
            ),
            HostEpoch::from_bytes([75; 16]),
            direct_checkpoint_limits(),
        );
        let runner = DurableRunner::new(DIRECT_SWITCHOVER_MAX_RUNNER_FUEL).unwrap();
        let mut now = 100;
        let terminal = loop {
            let pods = world.lock().unwrap().pods();
            let current_pods = pod_references(&pods);
            let mut adapter = DirectSwitchoverRunnerAdapter::new(
                &initial,
                &set,
                &current_pods,
                &api,
                host.store().clone(),
                now,
            )
            .unwrap();
            match runner
                .run(
                    &mut host,
                    &DirectSwitchoverWorkflow,
                    execution.clone(),
                    &mut adapter,
                    now,
                )
                .await
            {
                DurableRunnerOutcome::Terminal(terminal) => break terminal,
                DurableRunnerOutcome::Active { .. }
                | DurableRunnerOutcome::ReloadRequired { .. } => now += 1,
                other => panic!("unexpected bounded-redelivery outcome: {other:?}"),
            }
        };
        assert!(matches!(
            terminal,
            DirectSwitchoverTerminalRecord::Stopped { .. }
        ));
        assert_eq!(
            world
                .lock()
                .unwrap()
                .requests
                .iter()
                .filter(|request| action_sequence(&request.action_id) == 1)
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn direct_switchover_label_quarantine_never_redelivers() {
        let initial = direct_initial_operation("label-quarantine", snapshot(2), 2, 100).unwrap();
        let execution_id = ExecutionId::from_bytes([65; 16]);
        let execution = direct_execution_spec(execution_id, initial.clone()).unwrap();
        let world = Arc::new(StdMutex::new(world_for(&initial, Scenario::Success)));
        let api = TestApi {
            world: world.clone(),
            apply_labels: false,
        };
        let set = test_set(2);
        let mut host = DurableHost::new(
            MeasuredDurableCheckpointStore::with_decoder(
                execution_id,
                DurableCheckpointStore::InMemory(InMemoryCheckpointStore::new()),
                direct_checkpoint_measurement_decoder(),
            ),
            HostEpoch::from_bytes([66; 16]),
            direct_checkpoint_limits(),
        );
        let runner = DurableRunner::new(DIRECT_SWITCHOVER_MAX_RUNNER_FUEL).unwrap();
        let mut now = 100;
        for _ in 0..64 {
            let pods = world.lock().unwrap().pods();
            let current_pods = pod_references(&pods);
            let mut adapter = DirectSwitchoverRunnerAdapter::new(
                &initial,
                &set,
                &current_pods,
                &api,
                host.store().clone(),
                now,
            )
            .unwrap();
            match runner
                .run(
                    &mut host,
                    &DirectSwitchoverWorkflow,
                    execution.clone(),
                    &mut adapter,
                    now,
                )
                .await
            {
                DurableRunnerOutcome::Active { .. }
                | DurableRunnerOutcome::ReloadRequired { .. } => now += 1,
                DurableRunnerOutcome::Terminal(_) => {
                    panic!("non-applied label must remain quarantined")
                }
                other => panic!("unexpected label quarantine outcome: {other:?}"),
            }
            if world.lock().unwrap().label_patches.len() == 1 {
                for _ in 0..8 {
                    let pods = world.lock().unwrap().pods();
                    let current_pods = pod_references(&pods);
                    let mut adapter = DirectSwitchoverRunnerAdapter::new(
                        &initial,
                        &set,
                        &current_pods,
                        &api,
                        host.store().clone(),
                        now,
                    )
                    .unwrap();
                    assert!(matches!(
                        runner
                            .run(
                                &mut host,
                                &DirectSwitchoverWorkflow,
                                execution.clone(),
                                &mut adapter,
                                now,
                            )
                            .await,
                        DurableRunnerOutcome::Active { .. }
                    ));
                    now += 1;
                }
                assert_eq!(world.lock().unwrap().label_patches.len(), 1);
                return;
            }
        }
        panic!("direct switchover did not reach a label effect");
    }

    #[tokio::test]
    async fn direct_switchover_exposure_faults_grant_zero_false_permits() {
        for fault in [
            InMemoryFault::ConflictWithoutApply,
            InMemoryFault::OutcomeUnknownWithoutApply,
            InMemoryFault::OutcomeUnknownAfterApply,
            InMemoryFault::FailBeforeRequest(StoreErrorKind::Unavailable),
        ] {
            let initial = direct_initial_operation("exposure-fault", snapshot(2), 2, 100).unwrap();
            let execution_id = ExecutionId::from_bytes([67; 16]);
            let execution = direct_execution_spec(execution_id, initial.clone()).unwrap();
            let world = Arc::new(StdMutex::new(world_for(&initial, Scenario::Success)));
            let definition = DirectSwitchoverDefinition::from_initial(&initial).unwrap();
            let observations = observations(&world.lock().unwrap());
            let addressed = observations
                .iter()
                .map(|(id, observed)| (*id, observed.status.instance_id.clone()))
                .collect();
            let resolver = DirectSwitchoverPreparedActivityResolver::new(
                &definition,
                &observations,
                &addressed,
                100,
                Arc::new(AtomicI64::new(110)),
            );
            let backend = InMemoryCheckpointStore::new();
            backend.fail_next_compare_and_swap(fault);
            let mut host = DurableHost::new(
                MeasuredDurableCheckpointStore::with_decoder(
                    execution_id,
                    DurableCheckpointStore::InMemory(backend),
                    direct_checkpoint_measurement_decoder(),
                ),
                HostEpoch::from_bytes([68; 16]),
                direct_checkpoint_limits(),
            );
            assert!(!matches!(
                host.turn_and_expose_with(&DirectSwitchoverWorkflow, execution, &resolver,)
                    .await,
                HostOutcome::DispatchPermitted { .. }
            ));
            assert!(world.lock().unwrap().requests.is_empty());
        }
    }

    #[tokio::test]
    async fn direct_switchover_prepared_command_is_persisted_before_permit() {
        let initial =
            direct_initial_operation("prepared-before-permit", snapshot(2), 2, 100).unwrap();
        let execution_id = ExecutionId::from_bytes([69; 16]);
        let execution = direct_execution_spec(execution_id, initial.clone()).unwrap();
        let world = Arc::new(StdMutex::new(world_for(&initial, Scenario::Success)));
        let definition = DirectSwitchoverDefinition::from_initial(&initial).unwrap();
        let observations = observations(&world.lock().unwrap());
        let addressed = observations
            .iter()
            .map(|(id, observed)| (*id, observed.status.instance_id.clone()))
            .collect();
        let resolver = DirectSwitchoverPreparedActivityResolver::new(
            &definition,
            &observations,
            &addressed,
            100,
            Arc::new(AtomicI64::new(110)),
        );
        let backend = InMemoryCheckpointStore::new();
        let mut host = DurableHost::new(
            MeasuredDurableCheckpointStore::with_decoder(
                execution_id,
                DurableCheckpointStore::InMemory(backend.clone()),
                direct_checkpoint_measurement_decoder(),
            ),
            HostEpoch::from_bytes([70; 16]),
            direct_checkpoint_limits(),
        );
        let HostOutcome::DispatchPermitted { permit, .. } = host
            .turn_and_expose_with(&DirectSwitchoverWorkflow, execution, &resolver)
            .await
        else {
            panic!("direct prepared activity did not receive a permit");
        };
        assert!(world.lock().unwrap().requests.is_empty());
        let stored = backend.load(execution_id).await.unwrap().unwrap();
        let payload = stored
            .checkpoint()
            .decode_and_validate(
                &direct_execution_spec(execution_id, initial).unwrap(),
                direct_checkpoint_limits(),
            )
            .unwrap();
        let recorded = payload.active_activities().unwrap().last().unwrap();
        assert_eq!(recorded.spec(), permit.activity().spec());
        assert!(
            DirectActivity::decode(recorded.spec())
                .unwrap()
                .prepared_replica_command()
                .is_some()
        );
    }

    #[test]
    fn direct_switchover_replay_rejects_name_version_bound_input_and_command_drift() {
        let initial = direct_initial_operation("replay-drift", snapshot(2), 2, 100).unwrap();
        let definition = DirectSwitchoverDefinition::from_initial(&initial).unwrap();
        let world = world_for(&initial, Scenario::Success);
        let observations = observations(&world);
        let addressed = observations
            .iter()
            .map(|(id, observed)| (*id, observed.status.instance_id.clone()))
            .collect();
        let resolver = DirectSwitchoverPreparedActivityResolver::new(
            &definition,
            &observations,
            &addressed,
            100,
            Arc::new(AtomicI64::new(110)),
        );
        let logical_activity = DirectActivity::Replica {
            operation: super::super::prepare::DirectReplicaOperation::RevokeWrites,
            request: super::super::activities::ReplicaOperationRequest {
                contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
                execution_id: definition.execution_id.clone(),
                action_id: format!("{}:1", definition.execution_id),
                sequence: 1,
                target_id: definition.old_primary_id,
                target_instance_id: definition
                    .member(definition.old_primary_id)
                    .unwrap()
                    .instance_id
                    .clone(),
                expected_epoch: definition.previous_snapshot.epoch.clone(),
                desired_snapshot: definition.previous_snapshot.clone(),
                deadline_unix_seconds: definition.initial_deadline_unix_seconds,
                redelivery: 0,
                prepared_command: None,
            },
        };
        let logical = logical_activity.spec().unwrap();
        let prepared = resolver.resolve(&logical, None).unwrap();
        assert_ne!(prepared, logical);

        let changed_name = ActivitySpec::new(
            ActivityName::new("kuberic.switchover.changed", prepared.name().version()).unwrap(),
            prepared.input().clone(),
            prepared.max_result_bytes(),
        );
        let changed_version = ActivitySpec::new(
            ActivityName::new(
                prepared.name().name(),
                prepared.name().version().saturating_add(1),
            )
            .unwrap(),
            prepared.input().clone(),
            prepared.max_result_bytes(),
        );
        let changed_bound = ActivitySpec::new(
            prepared.name().clone(),
            prepared.input().clone(),
            prepared.max_result_bytes().saturating_add(1),
        );
        for drifted in [changed_name, changed_version, changed_bound] {
            assert_eq!(resolver.resolve(&logical, Some(&drifted)).unwrap(), logical);
        }

        let mut drifted = DirectActivity::decode(&prepared).unwrap();
        let DirectActivity::Replica { request, .. } = &mut drifted else {
            panic!("expected prepared replica activity");
        };
        request.action_id.push_str("-changed");
        let drifted = drifted.spec().unwrap();
        assert_eq!(resolver.resolve(&logical, Some(&drifted)).unwrap(), logical);

        let mut drifted = DirectActivity::decode(&prepared).unwrap();
        let DirectActivity::Replica { request, .. } = &mut drifted else {
            panic!("expected prepared replica activity");
        };
        let command = request.prepared_command.as_mut().unwrap();
        command.action_payload =
            kuberic_core::grpc::convert::encode_direct_correlated_action_payload(
                &DurableReplicaAction::UpdateEpoch {
                    epoch: Epoch::new(9, 9),
                },
            )
            .unwrap();
        command.action_signature = DurableReplicaAction::UpdateEpoch {
            epoch: Epoch::new(9, 9),
        }
        .signature();
        let drifted = drifted.spec().unwrap();
        assert_eq!(resolver.resolve(&logical, Some(&drifted)).unwrap(), logical);
    }

    #[test]
    fn direct_switchover_global_exact_bounds_and_one_over_are_enforced() {
        for (exact, validate) in [
            (
                DIRECT_SWITCHOVER_MAX_WORKFLOW_INPUT_BYTES,
                validate_direct_workflow_input_bytes as fn(usize) -> Result<(), String>,
            ),
            (
                DIRECT_SWITCHOVER_MAX_ACTIVE_ENCODED_BYTES,
                validate_direct_active_checkpoint_bytes,
            ),
            (
                DIRECT_SWITCHOVER_MAX_TERMINAL_ENCODED_BYTES,
                validate_direct_terminal_checkpoint_bytes,
            ),
            (
                DIRECT_SWITCHOVER_MAX_TERMINAL_PAYLOAD_BYTES as usize,
                validate_direct_terminal_payload_bytes,
            ),
            (
                DIRECT_SWITCHOVER_MAX_TRANSITION_FUEL,
                validate_direct_transition_fuel,
            ),
            (
                DIRECT_SWITCHOVER_MAX_RUNNER_FUEL,
                validate_direct_runner_fuel,
            ),
            (
                DIRECT_SWITCHOVER_MAX_ERROR_BYTES,
                validate_direct_error_bytes,
            ),
        ] {
            assert!(validate(exact).is_ok());
            assert!(validate(exact + 1).is_err());
        }
        assert!(validate_direct_runner_fuel(0).is_err());
        assert_eq!(direct_checkpoint_limits().max_activity_records(), 33);

        let execution_id = ExecutionId::from_bytes([73; 16]);
        let execution = ExecutionSpec::new(
            execution_id,
            ExactBytes::new(b"direct-bound".to_vec()),
            DIRECT_SWITCHOVER_MAX_TERMINAL_PAYLOAD_BYTES,
        );
        let contract = direct_execution_contract(execution);
        let activity = ActivitySpec::new(
            ActivityName::new("kuberic.switchover.bound-proof", 1).unwrap(),
            ExactBytes::new(b"{}".to_vec()),
            2,
        );
        let exact_records = (0..DIRECT_SWITCHOVER_MAX_ACTIVITY_RECORDS)
            .map(|sequence| {
                ActivityRecord::completed(
                    ActivitySequence::new(sequence as u64),
                    activity.clone(),
                    ExactBytes::new(b"{}".to_vec()),
                )
            })
            .collect::<Vec<_>>();
        let exact_payload = CheckpointPayload::active(contract.clone(), exact_records.clone());
        let exact_checkpoint =
            CheckpointEnvelope::encode_with_limits(&exact_payload, direct_checkpoint_limits())
                .unwrap();
        assert!(
            exact_checkpoint.encoded_len().unwrap() <= DIRECT_SWITCHOVER_MAX_ACTIVE_ENCODED_BYTES
        );

        let mut one_over_records = exact_records;
        one_over_records.push(ActivityRecord::completed(
            ActivitySequence::new(DIRECT_SWITCHOVER_MAX_ACTIVITY_RECORDS as u64),
            activity,
            ExactBytes::new(b"{}".to_vec()),
        ));
        assert!(matches!(
            CheckpointEnvelope::encode_with_limits(
                &CheckpointPayload::active(contract, one_over_records),
                direct_checkpoint_limits(),
            ),
            Err(CheckpointError::ActivityRecordLimitExceeded { .. })
        ));
    }

    #[test]
    fn direct_switchover_declared_max_fault_payloads_fit_global_byte_bounds() {
        fn push_records<A: DurableActivity>(records: &mut Vec<ActivityRecord>, count: usize) {
            for _ in 0..count {
                let sequence = ActivitySequence::new(records.len() as u64);
                records.push(ActivityRecord::completed(
                    sequence,
                    ActivitySpec::new(
                        ActivityName::new(A::NAME, A::VERSION).unwrap(),
                        ExactBytes::new(vec![b'x'; usize::try_from(A::MAX_INPUT_BYTES).unwrap()]),
                        A::MAX_RESULT_BYTES,
                    ),
                    ExactBytes::new(vec![b'x'; usize::try_from(A::MAX_RESULT_BYTES).unwrap()]),
                ));
            }
        }

        use super::super::activities::{
            AttestCompensatedTopologyActivity, CaptureFrozenLsnActivity,
            CompensateDistributeReplicaEpochActivity, CompensatePromoteOldPrimaryActivity,
            DemoteOldPrimaryActivity, InstallCompensationCatchUpConfigurationActivity,
            InstallCompensationCurrentConfigurationActivity, PromoteTargetActivity,
            RestoreOldPrimaryLabelActivity, RestoreTargetSecondaryLabelActivity,
            RevokeWritesActivity, WaitTargetCaughtUpActivity,
        };

        let mut records = Vec::new();
        push_records::<RevokeWritesActivity>(&mut records, 2);
        push_records::<CaptureFrozenLsnActivity>(&mut records, 1);
        push_records::<WaitTargetCaughtUpActivity>(&mut records, 1);
        push_records::<DemoteOldPrimaryActivity>(&mut records, 2);
        push_records::<PromoteTargetActivity>(&mut records, 2);
        push_records::<CompensatePromoteOldPrimaryActivity>(&mut records, 2);
        push_records::<CompensateDistributeReplicaEpochActivity>(&mut records, 16);
        push_records::<InstallCompensationCatchUpConfigurationActivity>(&mut records, 2);
        push_records::<InstallCompensationCurrentConfigurationActivity>(&mut records, 2);
        push_records::<RestoreOldPrimaryLabelActivity>(&mut records, 1);
        push_records::<RestoreTargetSecondaryLabelActivity>(&mut records, 1);
        push_records::<AttestCompensatedTopologyActivity>(&mut records, 1);
        assert_eq!(records.len(), DIRECT_SWITCHOVER_MAX_ACTIVITY_RECORDS);

        let execution = ExecutionSpec::new(
            ExecutionId::from_bytes([76; 16]),
            ExactBytes::new(vec![b'x'; DIRECT_SWITCHOVER_MAX_WORKFLOW_INPUT_BYTES]),
            DIRECT_SWITCHOVER_MAX_TERMINAL_PAYLOAD_BYTES,
        );
        let contract = direct_execution_contract(execution);
        let active = CheckpointEnvelope::encode_with_limits(
            &CheckpointPayload::active(contract.clone(), records),
            direct_checkpoint_limits(),
        )
        .unwrap();
        let active_bytes = active.encoded_len().unwrap();
        assert!(active_bytes <= DIRECT_SWITCHOVER_MAX_ACTIVE_ENCODED_BYTES);

        let terminal = CheckpointEnvelope::encode_with_limits(
            &CheckpointPayload::terminal(
                contract,
                TerminalOutcome::succeeded(ExactBytes::new(vec![
                    b'x';
                    DIRECT_SWITCHOVER_MAX_TERMINAL_PAYLOAD_BYTES
                        as usize
                ])),
                DIRECT_SWITCHOVER_MAX_ACTIVITY_RECORDS as u64,
            ),
            direct_checkpoint_limits(),
        )
        .unwrap();
        let terminal_bytes = terminal.encoded_len().unwrap();
        assert!(terminal_bytes <= DIRECT_SWITCHOVER_MAX_TERMINAL_ENCODED_BYTES);
        eprintln!(
            "direct switchover declared-bound projection: active={active_bytes} (headroom={}), terminal={terminal_bytes} (headroom={})",
            DIRECT_SWITCHOVER_MAX_ACTIVE_ENCODED_BYTES - active_bytes,
            DIRECT_SWITCHOVER_MAX_TERMINAL_ENCODED_BYTES - terminal_bytes,
        );
    }

    #[tokio::test]
    async fn direct_switchover_terminal_only_reload_recovers_named_activity_accounting() {
        let execution_id = ExecutionId::from_bytes([77; 16]);
        let execution = ExecutionSpec::new(
            execution_id,
            ExactBytes::new(b"direct-terminal-accounting".to_vec()),
            DIRECT_SWITCHOVER_MAX_TERMINAL_PAYLOAD_BYTES,
        );
        let contract = direct_execution_contract(execution);
        let accounting = DirectActivityAccounting::new(28, 5);
        let terminal = TerminalOutcome::succeeded(ExactBytes::new(
            serde_json::to_vec(&DirectSwitchoverTerminalRecord::Complete {
                snapshot: snapshot(9),
                compensated: true,
                reason: Some("promotion failed".to_string()),
                accounting: Some(accounting),
            })
            .unwrap(),
        ));
        let checkpoint = CheckpointEnvelope::encode_with_limits(
            &CheckpointPayload::terminal(contract, terminal, accounting.total().unwrap()),
            direct_checkpoint_limits(),
        )
        .unwrap();
        let backend = InMemoryCheckpointStore::new();
        let first = MeasuredDurableCheckpointStore::with_decoder(
            execution_id,
            DurableCheckpointStore::InMemory(backend.clone()),
            direct_checkpoint_measurement_decoder(),
        );
        assert!(matches!(
            first
                .compare_and_swap(execution_id, None, checkpoint)
                .await
                .unwrap(),
            kuberic_durable_execution::CasOutcome::Accepted(_)
        ));
        let reloaded = MeasuredDurableCheckpointStore::with_decoder(
            execution_id,
            DurableCheckpointStore::InMemory(backend),
            direct_checkpoint_measurement_decoder(),
        );
        assert!(reloaded.load(execution_id).await.unwrap().is_some());
        let measurements = reloaded.measurements();
        assert_eq!(measurements.completed_activity_count, Some(33));
        assert_eq!(measurements.completed_external_effect_count, Some(28));
        assert_eq!(measurements.completed_passive_observation_count, Some(5));
    }

    #[test]
    fn direct_switchover_phase_three_routes_production_to_the_direct_engine() {
        let reconciler = include_str!("../../reconciler.rs");
        assert!(reconciler.contains("let execution = native_execution_spec(reference)?;"));
        assert!(reconciler.contains("DirectSwitchoverRunnerAdapter::new"));
        assert!(reconciler.contains("&DirectSwitchoverWorkflow"));
        assert!(!reconciler.contains("NativeSwitchoverWorkflow"));
    }

    #[test]
    fn direct_switchover_expired_replica_preconditions_never_dispatch() {
        let initial = direct_initial_operation("set-deadline", snapshot(3), 2, 100).unwrap();
        let definition = DirectSwitchoverDefinition::from_initial(&initial).unwrap();
        let deadline = definition.initial_deadline_unix_seconds;
        for (operation, sequence) in replica_operation_cases() {
            let (activity, target_id) =
                replica_activity(&definition, operation, sequence, deadline);
            let mut world = world_for(&initial, Scenario::Success);
            set_replica_precondition(&mut world, &definition, operation, target_id);
            let available = observations(&world);
            assert!(
                matches!(
                    activity.evaluate(&definition, &available, deadline - 1),
                    Ok(DirectEvaluation::DispatchReplica { .. })
                ),
                "{operation:?}"
            );
            assert_eq!(
                observed_result_tag(activity.evaluate(&definition, &available, deadline)),
                "deadline_exceeded",
                "{operation:?}"
            );

            let mut unavailable = available;
            unavailable.remove(&target_id);
            assert!(
                matches!(
                    activity.evaluate(&definition, &unavailable, deadline - 1),
                    Ok(DirectEvaluation::AwaitEvidence)
                ),
                "{operation:?}"
            );
            assert_eq!(
                observed_result_tag(activity.evaluate(&definition, &unavailable, deadline)),
                "unavailable_at_deadline",
                "{operation:?}"
            );
        }
    }

    #[test]
    fn direct_switchover_expired_label_preconditions_never_dispatch() {
        let initial = direct_initial_operation("set-label-deadline", snapshot(3), 2, 100).unwrap();
        let definition = DirectSwitchoverDefinition::from_initial(&initial).unwrap();
        let deadline = definition.initial_deadline_unix_seconds;
        for operation in [
            DirectLabelOperation::PublishTargetPrimary,
            DirectLabelOperation::PublishOldPrimarySecondary,
            DirectLabelOperation::RestoreOldPrimary,
            DirectLabelOperation::RestoreTargetSecondary,
        ] {
            let (activity, target_id) = label_activity(&definition, operation, deadline);
            let mut world = world_for(&initial, Scenario::Success);
            set_label_precondition(&mut world, &definition, operation, target_id);
            let available = observations(&world);
            assert!(
                matches!(
                    activity.evaluate(&definition, &available, deadline - 1),
                    Ok(DirectEvaluation::DispatchLabel)
                ),
                "{operation:?}"
            );
            assert_eq!(
                observed_result_tag(activity.evaluate(&definition, &available, deadline)),
                "deadline_exceeded",
                "{operation:?}"
            );

            let mut unavailable = available;
            unavailable.remove(&target_id);
            assert!(
                matches!(
                    activity.evaluate(&definition, &unavailable, deadline - 1),
                    Ok(DirectEvaluation::AwaitEvidence)
                ),
                "{operation:?}"
            );
            assert_eq!(
                observed_result_tag(activity.evaluate(&definition, &unavailable, deadline)),
                "unavailable_at_deadline",
                "{operation:?}"
            );
        }
    }

    fn replica_operation_cases() -> [(DirectReplicaOperation, u32); 12] {
        [
            (DirectReplicaOperation::RevokeWrites, 1),
            (DirectReplicaOperation::DemoteOldPrimary, 2),
            (DirectReplicaOperation::PromoteTarget, 3),
            (DirectReplicaOperation::DistributeReplicaEpoch, 100),
            (
                DirectReplicaOperation::InstallTargetCatchUpConfiguration,
                1000,
            ),
            (DirectReplicaOperation::WaitTargetWriteQuorum, 1001),
            (
                DirectReplicaOperation::InstallTargetCurrentConfiguration,
                1002,
            ),
            (
                DirectReplicaOperation::RestorePreviousCurrentConfiguration,
                1500,
            ),
            (DirectReplicaOperation::CompensatePromoteOldPrimary, 2000),
            (
                DirectReplicaOperation::CompensateDistributeReplicaEpoch,
                2100,
            ),
            (
                DirectReplicaOperation::InstallCompensationCatchUpConfiguration,
                2001,
            ),
            (
                DirectReplicaOperation::InstallCompensationCurrentConfiguration,
                2002,
            ),
        ]
    }

    fn replica_activity(
        definition: &DirectSwitchoverDefinition,
        operation: DirectReplicaOperation,
        sequence: u32,
        deadline: i64,
    ) -> (DirectActivity, ReplicaId) {
        let previous = definition.previous_snapshot.clone();
        let target = definition.target_snapshot.clone();
        let compensation = definition.compensation_snapshot();
        let (target_id, expected_epoch, desired_snapshot) = match operation {
            DirectReplicaOperation::RevokeWrites => {
                (definition.old_primary_id, previous.epoch.clone(), previous)
            }
            DirectReplicaOperation::DemoteOldPrimary => {
                (definition.old_primary_id, target.epoch.clone(), target)
            }
            DirectReplicaOperation::PromoteTarget => {
                (definition.target_primary_id, previous.epoch.clone(), target)
            }
            DirectReplicaOperation::DistributeReplicaEpoch => {
                let index = usize::try_from(sequence - 100).unwrap();
                (
                    definition.normal_epoch_distribution_ids()[index],
                    target.epoch.clone(),
                    target,
                )
            }
            DirectReplicaOperation::InstallTargetCatchUpConfiguration
            | DirectReplicaOperation::WaitTargetWriteQuorum
            | DirectReplicaOperation::InstallTargetCurrentConfiguration => {
                (definition.target_primary_id, target.epoch.clone(), target)
            }
            DirectReplicaOperation::RestorePreviousCurrentConfiguration => {
                (definition.old_primary_id, previous.epoch.clone(), previous)
            }
            DirectReplicaOperation::CompensatePromoteOldPrimary
            | DirectReplicaOperation::InstallCompensationCatchUpConfiguration
            | DirectReplicaOperation::InstallCompensationCurrentConfiguration => (
                definition.old_primary_id,
                target.epoch.clone(),
                compensation,
            ),
            DirectReplicaOperation::CompensateDistributeReplicaEpoch => {
                let index = usize::try_from(sequence - 2100).unwrap();
                (
                    definition.compensation_epoch_distribution_ids()[index],
                    target.epoch.clone(),
                    compensation,
                )
            }
        };
        let request = ReplicaOperationRequest {
            contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
            execution_id: definition.execution_id.clone(),
            action_id: format!("{}:{sequence}", definition.execution_id),
            sequence,
            target_id,
            target_instance_id: definition.member(target_id).unwrap().instance_id.clone(),
            expected_epoch,
            desired_snapshot,
            deadline_unix_seconds: deadline,
            redelivery: 0,
            prepared_command: None,
        };
        (DirectActivity::Replica { operation, request }, target_id)
    }

    fn label_activity(
        definition: &DirectSwitchoverDefinition,
        operation: DirectLabelOperation,
        deadline: i64,
    ) -> (DirectActivity, ReplicaId) {
        let (sequence, target_id, desired_role) = match operation {
            DirectLabelOperation::PublishTargetPrimary => {
                (1003, definition.target_primary_id, "primary")
            }
            DirectLabelOperation::PublishOldPrimarySecondary => {
                (1004, definition.old_primary_id, "secondary")
            }
            DirectLabelOperation::RestoreOldPrimary => (2003, definition.old_primary_id, "primary"),
            DirectLabelOperation::RestoreTargetSecondary => {
                (2004, definition.target_primary_id, "secondary")
            }
        };
        let request = LabelOperationRequest {
            contract_version: DIRECT_SWITCHOVER_CONTRACT_VERSION,
            execution_id: definition.execution_id.clone(),
            action_id: format!("{}:{sequence}", definition.execution_id),
            sequence,
            target_id,
            target_instance_id: definition.member(target_id).unwrap().instance_id.clone(),
            desired_role: desired_role.to_string(),
            deadline_unix_seconds: deadline,
            prepared_command: None,
        };
        (DirectActivity::Label { operation, request }, target_id)
    }

    fn set_replica_precondition(
        world: &mut TestWorld,
        definition: &DirectSwitchoverDefinition,
        operation: DirectReplicaOperation,
        target_id: ReplicaId,
    ) {
        let previous_epoch = Epoch::new(
            definition.previous_snapshot.epoch.data_loss_number,
            definition.previous_snapshot.epoch.configuration_number,
        );
        let target_epoch = Epoch::new(
            definition.target_snapshot.epoch.data_loss_number,
            definition.target_snapshot.epoch.configuration_number,
        );
        let previous_current = configuration_status(
            ReplicaConfigurationMode::Current,
            &definition.previous_snapshot,
        );
        let target_catch_up = configuration_status(
            ReplicaConfigurationMode::CatchUp,
            &definition.target_snapshot,
        );
        let compensation_catch_up = configuration_status(
            ReplicaConfigurationMode::CatchUp,
            &definition.compensation_snapshot(),
        );
        let status = world.statuses.get_mut(&target_id).unwrap();
        status.agent.current_action = None;
        status.agent.retained_terminal_actions.clear();
        match operation {
            DirectReplicaOperation::RevokeWrites => {
                status.role = Role::Primary;
                status.epoch = previous_epoch;
                status.write_status = AccessStatus::Granted;
                status.configuration = Some(previous_current);
            }
            DirectReplicaOperation::DemoteOldPrimary => {
                status.role = Role::Primary;
                status.epoch = previous_epoch;
                status.write_status = AccessStatus::ReconfigurationPending;
                status.configuration = Some(previous_current);
            }
            DirectReplicaOperation::PromoteTarget
            | DirectReplicaOperation::DistributeReplicaEpoch
            | DirectReplicaOperation::CompensateDistributeReplicaEpoch => {
                status.role = Role::ActiveSecondary;
                status.epoch = previous_epoch;
                status.write_status = AccessStatus::NotPrimary;
                status.configuration = None;
            }
            DirectReplicaOperation::InstallTargetCatchUpConfiguration => {
                status.role = Role::Primary;
                status.epoch = target_epoch;
                status.write_status = AccessStatus::Granted;
                status.configuration = None;
            }
            DirectReplicaOperation::WaitTargetWriteQuorum
            | DirectReplicaOperation::InstallTargetCurrentConfiguration => {
                status.role = Role::Primary;
                status.epoch = target_epoch;
                status.write_status = AccessStatus::Granted;
                status.configuration = Some(target_catch_up);
            }
            DirectReplicaOperation::RestorePreviousCurrentConfiguration => {
                status.role = Role::Primary;
                status.epoch = previous_epoch;
                status.write_status = AccessStatus::ReconfigurationPending;
                status.configuration = Some(previous_current);
            }
            DirectReplicaOperation::CompensatePromoteOldPrimary => {
                status.role = Role::ActiveSecondary;
                status.epoch = target_epoch;
                status.write_status = AccessStatus::NotPrimary;
                status.configuration = None;
            }
            DirectReplicaOperation::InstallCompensationCatchUpConfiguration => {
                status.role = Role::Primary;
                status.epoch = target_epoch;
                status.write_status = AccessStatus::Granted;
                status.configuration = Some(previous_current);
            }
            DirectReplicaOperation::InstallCompensationCurrentConfiguration => {
                status.role = Role::Primary;
                status.epoch = target_epoch;
                status.write_status = AccessStatus::Granted;
                status.configuration = Some(compensation_catch_up);
            }
        }
    }

    fn set_label_precondition(
        world: &mut TestWorld,
        definition: &DirectSwitchoverDefinition,
        operation: DirectLabelOperation,
        target_id: ReplicaId,
    ) {
        let target_epoch = Epoch::new(
            definition.target_snapshot.epoch.data_loss_number,
            definition.target_snapshot.epoch.configuration_number,
        );
        let status = world.statuses.get_mut(&target_id).unwrap();
        status.epoch = target_epoch;
        match operation {
            DirectLabelOperation::PublishTargetPrimary => {
                status.role = Role::Primary;
                world.labels.insert(target_id, "secondary".to_string());
            }
            DirectLabelOperation::PublishOldPrimarySecondary => {
                status.role = Role::ActiveSecondary;
                world.labels.insert(target_id, "primary".to_string());
            }
            DirectLabelOperation::RestoreOldPrimary => {
                status.role = Role::Primary;
                world.labels.insert(target_id, "secondary".to_string());
            }
            DirectLabelOperation::RestoreTargetSecondary => {
                status.role = Role::ActiveSecondary;
                world.labels.insert(target_id, "primary".to_string());
            }
        }
    }

    fn observed_result_tag(result: Result<DirectEvaluation, String>) -> String {
        let DirectEvaluation::Observe(result) = result.unwrap() else {
            panic!("expired activity must return an observation without dispatch");
        };
        serde_json::from_slice::<serde_json::Value>(result.as_slice())
            .unwrap()
            .get("result")
            .and_then(serde_json::Value::as_str)
            .unwrap()
            .to_string()
    }

    fn snapshot(member_count: usize) -> StablePartitionSnapshotStatus {
        StablePartitionSnapshotStatus {
            epoch: EpochStatus {
                data_loss_number: 4,
                configuration_number: 8,
            },
            primary_id: 1,
            members: (1..=member_count)
                .map(|id| StableReplicaSnapshotStatus {
                    id: i64::try_from(id).unwrap(),
                    instance_id: format!("pod-{id}-uid"),
                    role: if id == 1 {
                        StableReplicaRoleStatus::Primary
                    } else {
                        StableReplicaRoleStatus::ActiveSecondary
                    },
                    election_metadata: None,
                })
                .collect(),
            write_quorum: u32::try_from(member_count / 2 + 1).unwrap(),
        }
    }

    fn world_for(initial: &crate::crd::DurableOperationStatus, scenario: Scenario) -> TestWorld {
        let previous = initial.previous_snapshot.as_ref().unwrap();
        let previous_configuration =
            configuration_status(ReplicaConfigurationMode::Current, previous);
        let statuses = previous
            .members
            .iter()
            .map(|member| {
                let role = match member.role {
                    StableReplicaRoleStatus::Primary => Role::Primary,
                    StableReplicaRoleStatus::ActiveSecondary => Role::ActiveSecondary,
                };
                let progress = if scenario == Scenario::PrePromotionCompensation && member.id == 2 {
                    0
                } else {
                    100
                };
                (
                    member.id,
                    ReplicaStatusInfo {
                        instance_id: ReplicaInstanceId::new(member.instance_id.clone()),
                        role,
                        epoch: Epoch::new(
                            previous.epoch.data_loss_number,
                            previous.epoch.configuration_number,
                        ),
                        current_progress: progress,
                        catch_up_capability: Some(100),
                        committed_lsn: 100,
                        healthy: true,
                        write_status: if role == Role::Primary {
                            AccessStatus::Granted
                        } else {
                            AccessStatus::NotPrimary
                        },
                        configuration: (role == Role::Primary)
                            .then(|| previous_configuration.clone()),
                        election_configuration: None,
                        deactivation_info: None,
                        active_replica_connections: Vec::new(),
                        build_observation: None,
                        agent: ReplicaAgentStatus {
                            protocol_version:
                                kuberic_core::replica_agent::CORRELATED_CONTROL_PROTOCOL_VERSION,
                            lifecycle_peer_protocol_version: 1,
                            generation: AgentGeneration::parse(format!("{:032x}", member.id))
                                .unwrap(),
                            control_version: AgentControlVersion::new(1),
                            current_action: None,
                            retained_terminal_actions: Vec::new(),
                            local_faults: Vec::new(),
                        },
                    },
                )
            })
            .collect();
        let labels = previous
            .members
            .iter()
            .map(|member| {
                (
                    member.id,
                    if member.id == previous.primary_id {
                        "primary".to_string()
                    } else {
                        "secondary".to_string()
                    },
                )
            })
            .collect();
        let mut world = TestWorld {
            statuses,
            labels,
            ..Default::default()
        };
        if scenario == Scenario::PostPromotionCompensation {
            world.fail_sequences.insert(3);
        }
        world
    }

    fn test_set(member_count: usize) -> KubericSet {
        KubericSet {
            metadata: ObjectMeta {
                name: Some("set".to_string()),
                namespace: Some("default".to_string()),
                uid: Some("set-uid".to_string()),
                ..Default::default()
            },
            spec: KubericSetSpec {
                replicas: i32::try_from(member_count).unwrap(),
                min_replicas: 1,
                image: "test:latest".to_string(),
                failover_delay: 0,
                switchover_delay: 30,
                port: 8080,
                control_port: 9090,
                data_port: 9091,
                storage: "256Mi".to_string(),
                pvc_retention_policy: PvcRetentionPolicy::Delete,
            },
            status: None,
        }
    }

    fn pod_references(pods: &[Pod]) -> Vec<(ReplicaId, ReplicaInstanceId, &Pod)> {
        pods.iter()
            .map(|pod| {
                let id = pod
                    .metadata
                    .labels
                    .as_ref()
                    .unwrap()
                    .get("kuberic.io/replica-id")
                    .unwrap()
                    .parse()
                    .unwrap();
                (
                    id,
                    ReplicaInstanceId::new(pod.metadata.uid.clone().unwrap()),
                    pod,
                )
            })
            .collect()
    }

    fn observations(world: &TestWorld) -> OperationObservations {
        world
            .statuses
            .iter()
            .map(|(id, status)| {
                (
                    *id,
                    crate::durable::ReplicaObservation {
                        status: status.clone(),
                        control_address: format!("http://set-{id}:9090"),
                        replicator_address: format!("http://set-{id}:9091"),
                        pod_name: format!("set-{id}"),
                        pod_role_label: world.labels.get(id).cloned(),
                    },
                )
            })
            .collect()
    }

    fn action_sequence(action_id: &str) -> u32 {
        action_id.rsplit(':').next().unwrap().parse().unwrap()
    }

    fn apply_action(
        world: &mut TestWorld,
        request: &CorrelatedControlActionRequest,
    ) -> kuberic_core::Result<CorrelatedControlActionAcknowledgement> {
        let status = world.statuses.get_mut(&request.target_replica_id).unwrap();
        match &request.action {
            DurableReplicaAction::RevokeWriteStatus => {
                status.write_status = AccessStatus::ReconfigurationPending;
            }
            DurableReplicaAction::ChangeRole { epoch, role } => {
                status.epoch = *epoch;
                status.role = *role;
                status.write_status = if *role == Role::Primary {
                    AccessStatus::ReconfigurationPending
                } else {
                    AccessStatus::NotPrimary
                };
            }
            DurableReplicaAction::UpdateEpoch { epoch } => {
                status.epoch = *epoch;
            }
            DurableReplicaAction::UpdateCatchUpConfiguration { current, .. } => {
                status.configuration = Some(ReplicaConfigurationStatus::from_config(
                    ReplicaConfigurationMode::CatchUp,
                    current,
                ));
            }
            DurableReplicaAction::WaitForCatchUpQuorum { .. } => {}
            DurableReplicaAction::UpdateCurrentConfiguration { current } => {
                status.configuration = Some(ReplicaConfigurationStatus::from_config(
                    ReplicaConfigurationMode::Current,
                    current,
                ));
                if status.role == Role::Primary {
                    status.write_status = AccessStatus::Granted;
                }
            }
            other => {
                return Err(KubericError::Internal(
                    format!("unexpected direct test action {other:?}").into(),
                ));
            }
        }
        status.agent.control_version =
            AgentControlVersion::new(status.agent.control_version.value().saturating_add(1));
        let action = DurableActionObservation {
            action_id: request.action_id.clone(),
            signature: request.input_signature.clone(),
            state: DurableActionState::Completed,
            error_class: None,
            error: None,
            result: None,
            add_replica_progress: None,
            remove_replica_progress: None,
        };
        let observation = CorrelatedActionObservation {
            generation: status.agent.generation.clone(),
            control_version: status.agent.control_version,
            action,
        };
        status
            .agent
            .retained_terminal_actions
            .push(observation.clone());
        Ok(CorrelatedControlActionAcknowledgement { observation })
    }

    fn configuration_status(
        mode: ReplicaConfigurationMode,
        snapshot: &StablePartitionSnapshotStatus,
    ) -> ReplicaConfigurationStatus {
        let mut members = snapshot
            .members
            .iter()
            .filter(|member| member.id != snapshot.primary_id)
            .map(|member| ReplicaConfigurationMemberStatus {
                id: member.id,
                instance_id: ReplicaInstanceId::new(member.instance_id.clone()),
                role: Role::ActiveSecondary,
            })
            .collect::<Vec<_>>();
        members.sort_by_key(|member| member.id);
        ReplicaConfigurationStatus {
            mode,
            members,
            write_quorum: snapshot.write_quorum,
        }
    }
}
