use std::collections::VecDeque;

use tokio::sync::{mpsc, oneshot, watch};
use tracing::{debug, info, warn};

use crate::Result;
use crate::error::KubericError;
use crate::pod::{
    RuntimeControlSnapshot, RuntimeEffect, RuntimeEffectCommand, RuntimeEffectResult,
};
use crate::types::{
    AccessStatus, AgentControlVersion, AgentGeneration, CancellationToken,
    CorrelatedActionObservation, CorrelatedControlActionAcknowledgement,
    CorrelatedControlActionRequest, DurableActionErrorClass, DurableActionObservation,
    DurableActionResult, DurableActionState, DurableReplicaAction, FaultType, LocalFaultRecord,
    ReplicaAgentStatus, ReplicaId, ReplicaInstanceId, ReplicaStatusInfo,
};

pub const TERMINAL_RETENTION: usize = 16;
pub const FAULT_RETENTION: usize = 16;
const MAX_ERROR_BYTES: usize = 1024;
pub const CORRELATED_CONTROL_PROTOCOL_VERSION: u32 = 1;

/// Transport-facing commands accepted by the pod-local replica agent.
pub enum AgentCommand {
    ExecuteCorrelatedControlAction {
        request: Box<CorrelatedControlActionRequest>,
        reply: oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>,
    },
    GetStatus {
        reply: oneshot::Sender<ReplicaStatusInfo>,
    },
}

struct ActiveCorrelated {
    execution_id: RuntimeEffectExecutionId,
    observation: CorrelatedActionObservation,
    reply: Option<oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RuntimeEffectExecutionId {
    generation: AgentGeneration,
    sequence: u64,
}

struct RuntimeCompletion {
    execution_id: RuntimeEffectExecutionId,
    result: Result<RuntimeEffectResult>,
}

/// Pod-local correlated-control owner between gRPC and ordered runtime effects.
pub struct ReplicaAgent {
    replica_id: ReplicaId,
    instance_id: ReplicaInstanceId,
    generation: AgentGeneration,
    control_version: AgentControlVersion,
    execution_sequence: u64,
    fault_sequence: u64,
    command_rx: mpsc::Receiver<AgentCommand>,
    runtime_tx: mpsc::Sender<RuntimeEffectCommand>,
    runtime_status_rx: watch::Receiver<RuntimeControlSnapshot>,
    fault_rx: mpsc::Receiver<FaultType>,
    fault_rx_open: bool,
    command_rx_open: bool,
    shutdown: CancellationToken,
    completion_tx: mpsc::UnboundedSender<RuntimeCompletion>,
    completion_rx: mpsc::UnboundedReceiver<RuntimeCompletion>,
    active: Option<ActiveCorrelated>,
    terminals: VecDeque<CorrelatedActionObservation>,
    faults: VecDeque<LocalFaultRecord>,
    shutting_down: bool,
}

impl ReplicaAgent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        replica_id: ReplicaId,
        instance_id: ReplicaInstanceId,
        command_rx: mpsc::Receiver<AgentCommand>,
        runtime_tx: mpsc::Sender<RuntimeEffectCommand>,
        runtime_status_rx: watch::Receiver<RuntimeControlSnapshot>,
        fault_rx: mpsc::Receiver<FaultType>,
        shutdown: CancellationToken,
    ) -> Self {
        let generation = AgentGeneration::generate();
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();
        info!(
            replica_id,
            instance_id = %instance_id,
            agent_generation = %generation,
            "ReplicaAgent process generation created"
        );
        Self {
            replica_id,
            instance_id,
            generation,
            control_version: AgentControlVersion::default(),
            execution_sequence: 0,
            fault_sequence: 0,
            command_rx,
            runtime_tx,
            runtime_status_rx,
            fault_rx,
            fault_rx_open: true,
            command_rx_open: true,
            shutdown,
            completion_tx,
            completion_rx,
            active: None,
            terminals: VecDeque::with_capacity(TERMINAL_RETENTION),
            faults: VecDeque::with_capacity(FAULT_RETENTION),
            shutting_down: false,
        }
    }

    pub async fn serve(mut self) {
        info!(
            replica_id = self.replica_id,
            agent_generation = %self.generation,
            "ReplicaAgent serve loop started"
        );
        loop {
            if self.shutting_down && self.active.is_none() {
                break;
            }
            tokio::select! {
                biased;
                completion = self.completion_rx.recv() => {
                    if let Some(completion) = completion {
                        self.handle_completion(completion);
                    }
                }
                fault = self.fault_rx.recv(), if self.fault_rx_open => {
                    match fault {
                        Some(fault) => self.record_fault(fault),
                        None => self.fault_rx_open = false,
                    }
                }
                command = self.command_rx.recv(), if self.command_rx_open => {
                    match command {
                        Some(AgentCommand::GetStatus { reply }) => {
                            let _ = reply.send(self.status());
                        }
                        Some(command) if self.shutting_down => {
                            reject_agent_command(command, KubericError::Closed);
                        }
                        Some(AgentCommand::ExecuteCorrelatedControlAction { request, reply }) => {
                            self.accept_correlated(*request, reply);
                        }
                        None => {
                            self.command_rx_open = false;
                            self.begin_shutdown();
                        }
                    }
                }
                _ = self.shutdown.cancelled(), if !self.shutting_down => {
                    self.begin_shutdown();
                }
            }
        }
        info!(
            replica_id = self.replica_id,
            agent_generation = %self.generation,
            "ReplicaAgent serve loop stopped"
        );
    }

    fn accept_correlated(
        &mut self,
        request: CorrelatedControlActionRequest,
        reply: oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>,
    ) {
        if request.protocol_version != CORRELATED_CONTROL_PROTOCOL_VERSION {
            let _ = reply.send(Err(KubericError::UnsupportedControlProtocolVersion {
                got: request.protocol_version,
            }));
            return;
        }
        if request.action_id.is_empty() {
            let _ = reply.send(Err(KubericError::InvalidCorrelatedActionId));
            return;
        }
        if request.target_replica_id != self.replica_id
            || request.target_instance_id != self.instance_id
        {
            let _ = reply.send(Err(KubericError::CorrelatedTargetMismatch {
                expected_id: self.replica_id,
                expected_instance: self.instance_id.clone(),
                actual_id: request.target_replica_id,
                actual_instance: request.target_instance_id,
            }));
            return;
        }
        if request.expected_agent_generation != self.generation {
            let _ = reply.send(Err(KubericError::StaleAgentGeneration {
                expected: request.expected_agent_generation,
                current: self.generation.clone(),
            }));
            return;
        }
        let actual_signature = request.action.signature();
        if request.input_signature != actual_signature {
            let _ = reply.send(Err(KubericError::ActionSignatureMismatch {
                action_id: request.action_id,
            }));
            return;
        }
        if let Some(observed) = self.find_correlated(&request.action_id) {
            if observed.action.signature != request.input_signature {
                warn!(
                    action_id = request.action_id,
                    agent_generation = %self.generation,
                    "rejecting correlated action ID reused with different input"
                );
                let _ = reply.send(Err(KubericError::ActionIdConflict {
                    action_id: request.action_id,
                }));
            } else {
                debug!(
                    action_id = request.action_id,
                    state = ?observed.action.state,
                    agent_generation = %self.generation,
                    "replaying correlated action observation"
                );
                send_observation(reply, observed.clone());
            }
            return;
        }
        if request.expected_control_version != self.control_version {
            let error = if request.expected_control_version < self.control_version {
                KubericError::CorrelatedContinuityUnavailable {
                    action_id: request.action_id,
                }
            } else {
                KubericError::StaleAgentControlVersion {
                    expected: request.expected_control_version,
                    current: self.control_version,
                }
            };
            let _ = reply.send(Err(error));
            return;
        }
        if self.active.is_some() {
            let _ = reply.send(Err(KubericError::AgentBusy));
            return;
        }
        let runtime_epoch = self.runtime_status_rx.borrow().epoch;
        if request.observed_runtime_epoch != runtime_epoch {
            let _ = reply.send(Err(KubericError::StaleEpoch {
                got: request.observed_runtime_epoch,
                current: runtime_epoch,
            }));
            return;
        }
        if self.shutting_down {
            let _ = reply.send(Err(KubericError::Closed));
            return;
        }

        let control_version = self.control_version.advance();
        let mut observation = CorrelatedActionObservation {
            generation: self.generation.clone(),
            control_version,
            action: DurableActionObservation {
                action_id: request.action_id.clone(),
                signature: request.input_signature,
                state: DurableActionState::Scheduled,
                error_class: None,
                error: None,
                result: None,
            },
        };
        debug!(
            action_id = request.action_id,
            control_version = control_version.value(),
            agent_generation = %self.generation,
            "accepted correlated action"
        );

        if let DurableReplicaAction::OnDataLoss { epoch } = request.action {
            let runtime = self.runtime_status_rx.borrow();
            if runtime.epoch != epoch {
                let error = format!(
                    "data-loss action epoch {:?} does not match runtime epoch {:?}",
                    epoch, runtime.epoch
                );
                drop(runtime);
                observation.action.state = DurableActionState::Failed;
                observation.action.error_class = Some(DurableActionErrorClass::StaleEpoch);
                observation.action.error = Some(normalize_error(&error));
                send_observation(reply, observation.clone());
                self.retain_terminal(observation);
                warn!(
                    action_id = request.action_id,
                    expected_epoch = ?epoch,
                    runtime_epoch = ?runtime_epoch,
                    "rejecting durable data-loss action at mismatched epoch"
                );
                return;
            }
            if runtime.partition_state.is_none() {
                drop(runtime);
                self.fail_before_dispatch(observation, reply, "replicator not opened");
                return;
            }
            drop(runtime);
            observation.action.state = DurableActionState::InProgress;
            send_observation(reply, observation.clone());
            self.start_effect(
                observation,
                None,
                RuntimeEffect::OnDataLoss {
                    expected_epoch: epoch,
                },
            );
            return;
        }

        if let DurableReplicaAction::BuildReplica { replica } = request.action {
            if self.runtime_status_rx.borrow().partition_state.is_none() {
                self.fail_before_dispatch(observation, reply, "replicator not opened");
                return;
            }
            observation.action.state = DurableActionState::InProgress;
            send_observation(reply, observation.clone());
            self.start_effect(observation, None, RuntimeEffect::BuildReplica { replica });
            return;
        }

        let effect = correlated_runtime_effect(request.action);
        observation.action.state = DurableActionState::InProgress;
        self.start_effect(observation, Some(reply), effect);
    }

    fn start_effect(
        &mut self,
        observation: CorrelatedActionObservation,
        reply: Option<oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>>,
        effect: RuntimeEffect,
    ) {
        let execution_id = self.next_execution_id();
        self.dispatch_runtime(execution_id.clone(), effect);
        self.active = Some(ActiveCorrelated {
            execution_id,
            observation,
            reply,
        });
    }

    fn fail_before_dispatch(
        &mut self,
        mut observation: CorrelatedActionObservation,
        reply: oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>,
        message: &'static str,
    ) {
        observation.action.state = DurableActionState::Failed;
        observation.action.error_class = Some(DurableActionErrorClass::Internal);
        observation.action.error = Some(message.to_string());
        send_observation(reply, observation.clone());
        self.retain_terminal(observation);
    }

    fn dispatch_runtime(&self, execution_id: RuntimeEffectExecutionId, effect: RuntimeEffect) {
        let runtime_tx = self.runtime_tx.clone();
        let completion_tx = self.completion_tx.clone();
        tokio::spawn(async move {
            let (reply_tx, reply_rx) = oneshot::channel();
            let result = match runtime_tx
                .send(RuntimeEffectCommand {
                    effect,
                    reply: reply_tx,
                })
                .await
            {
                Ok(()) => reply_rx.await.unwrap_or_else(|_| Err(KubericError::Closed)),
                Err(_) => Err(KubericError::Closed),
            };
            let _ = completion_tx.send(RuntimeCompletion {
                execution_id,
                result,
            });
        });
    }

    fn handle_completion(&mut self, completion: RuntimeCompletion) {
        let Some(mut active) = self.active.take() else {
            warn!(
                execution_sequence = completion.execution_id.sequence,
                "discarding runtime completion without active action"
            );
            return;
        };
        if active.execution_id != completion.execution_id {
            warn!("discarding mismatched correlated runtime completion");
            self.active = Some(active);
            return;
        }

        match completion.result {
            Ok(RuntimeEffectResult::Unit) => {
                active.observation.action.state = DurableActionState::Completed;
            }
            Ok(RuntimeEffectResult::DataLoss(result)) => {
                active.observation.action.state = DurableActionState::Completed;
                active.observation.action.result = Some(DurableActionResult::DataLoss(result));
            }
            Err(error) => {
                active.observation.action.state = DurableActionState::Failed;
                active.observation.action.error_class = Some(terminal_error_class(&error));
                active.observation.action.error = Some(normalize_error(&error.to_string()));
            }
        }
        if active.observation.action.state == DurableActionState::Failed {
            warn!(
                action_id = active.observation.action.action_id,
                agent_generation = %self.generation,
                error = active.observation.action.error.as_deref().unwrap_or_default(),
                "correlated action failed"
            );
        } else {
            debug!(
                action_id = active.observation.action.action_id,
                state = ?active.observation.action.state,
                agent_generation = %self.generation,
                "correlated action reached terminal state"
            );
        }
        if let Some(reply) = active.reply {
            send_observation(reply, active.observation.clone());
        }
        self.retain_terminal(active.observation);
    }

    fn begin_shutdown(&mut self) {
        if self.shutting_down {
            return;
        }
        self.shutting_down = true;
        info!(
            replica_id = self.replica_id,
            agent_generation = %self.generation,
            "ReplicaAgent stopping mutation admission"
        );
    }

    fn record_fault(&mut self, fault_type: FaultType) {
        self.fault_sequence = self.fault_sequence.saturating_add(1);
        if self.faults.len() == FAULT_RETENTION {
            self.faults.pop_front();
        }
        self.faults.push_back(LocalFaultRecord {
            sequence: self.fault_sequence,
            fault_type,
        });
        warn!(
            fault_sequence = self.fault_sequence,
            ?fault_type,
            agent_generation = %self.generation,
            "replica reported a local fault"
        );
    }

    fn status(&self) -> ReplicaStatusInfo {
        let runtime = self.runtime_status_rx.borrow().clone();
        let state = runtime.partition_state.as_deref();
        ReplicaStatusInfo {
            instance_id: runtime.instance_id,
            role: runtime.role,
            epoch: runtime.epoch,
            current_progress: state.map_or(0, |state| state.current_progress()),
            catch_up_capability: state.and_then(|state| state.observed_catch_up_capability()),
            committed_lsn: state.map_or(0, |state| state.committed_lsn()),
            healthy: state.is_some(),
            write_status: state.map_or(AccessStatus::NotPrimary, |state| state.write_status()),
            configuration: runtime.configuration,
            election_configuration: runtime.election_configuration,
            deactivation_info: runtime.deactivation_info,
            active_replica_connections: state
                .map_or_else(Vec::new, |state| state.active_replica_connections()),
            agent: ReplicaAgentStatus {
                protocol_version: CORRELATED_CONTROL_PROTOCOL_VERSION,
                generation: self.generation.clone(),
                control_version: self.control_version,
                current_action: self
                    .active
                    .as_ref()
                    .map(|active| active.observation.clone()),
                retained_terminal_actions: self.terminals.iter().cloned().collect(),
                local_faults: self.faults.iter().copied().collect(),
            },
        }
    }

    fn find_correlated(&self, action_id: &str) -> Option<&CorrelatedActionObservation> {
        self.active
            .as_ref()
            .filter(|active| active.observation.action.action_id == action_id)
            .map(|active| &active.observation)
            .or_else(|| {
                self.terminals
                    .iter()
                    .rev()
                    .find(|entry| entry.action.action_id == action_id)
            })
    }

    fn retain_terminal(&mut self, observation: CorrelatedActionObservation) {
        if self.terminals.len() == TERMINAL_RETENTION {
            self.terminals.pop_front();
        }
        self.terminals.push_back(observation);
    }

    fn next_execution_id(&mut self) -> RuntimeEffectExecutionId {
        self.execution_sequence = self.execution_sequence.saturating_add(1);
        RuntimeEffectExecutionId {
            generation: self.generation.clone(),
            sequence: self.execution_sequence,
        }
    }
}

fn correlated_runtime_effect(action: DurableReplicaAction) -> RuntimeEffect {
    match action {
        DurableReplicaAction::Open { mode } => RuntimeEffect::Open { mode },
        DurableReplicaAction::Close => RuntimeEffect::Close,
        DurableReplicaAction::RevokeWriteStatus => RuntimeEffect::RevokeWriteStatus,
        DurableReplicaAction::ChangeRole { epoch, role } => {
            RuntimeEffect::ChangeRole { epoch, role }
        }
        DurableReplicaAction::UpdateEpoch { epoch } => RuntimeEffect::UpdateEpoch { epoch },
        DurableReplicaAction::UpdateCatchUpConfiguration { current, previous } => {
            RuntimeEffect::UpdateCatchUpConfiguration { current, previous }
        }
        DurableReplicaAction::WaitForCatchUpQuorum { mode } => {
            RuntimeEffect::WaitForCatchUpQuorum { mode }
        }
        DurableReplicaAction::UpdateCurrentConfiguration { current } => {
            RuntimeEffect::UpdateCurrentConfiguration { current }
        }
        DurableReplicaAction::BuildReplica { replica } => RuntimeEffect::BuildReplica { replica },
        DurableReplicaAction::RemoveReplica {
            replica_id,
            instance_id,
        } => RuntimeEffect::RemoveReplica {
            replica_id,
            instance_id,
        },
        DurableReplicaAction::OnDataLoss { epoch } => RuntimeEffect::OnDataLoss {
            expected_epoch: epoch,
        },
        DurableReplicaAction::RecordElectionConfiguration { configuration } => {
            RuntimeEffect::RecordElectionConfiguration { configuration }
        }
    }
}

fn reject_agent_command(command: AgentCommand, error: KubericError) {
    if let AgentCommand::ExecuteCorrelatedControlAction { reply, .. } = command {
        let _ = reply.send(Err(error));
    }
}

fn send_observation(
    reply: oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>,
    observation: CorrelatedActionObservation,
) {
    let _ = reply.send(Ok(CorrelatedControlActionAcknowledgement { observation }));
}

fn normalize_error(error: &str) -> String {
    if error.len() <= MAX_ERROR_BYTES {
        return error.to_string();
    }

    let mut boundary = MAX_ERROR_BYTES;
    while !error.is_char_boundary(boundary) {
        boundary -= 1;
    }
    error[..boundary].to_string()
}

fn terminal_error_class(error: &KubericError) -> DurableActionErrorClass {
    match error {
        KubericError::NotPrimary => DurableActionErrorClass::NotPrimary,
        KubericError::NoWriteQuorum => DurableActionErrorClass::NoWriteQuorum,
        KubericError::ReconfigurationPending => DurableActionErrorClass::ReconfigurationPending,
        KubericError::StaleEpoch { .. } => DurableActionErrorClass::StaleEpoch,
        KubericError::Cancelled => DurableActionErrorClass::Cancelled,
        KubericError::Closed => DurableActionErrorClass::Closed,
        _ => DurableActionErrorClass::Internal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::handles::PartitionState;
    use crate::types::{Epoch, FaultType, Role};

    fn test_agent(
        epoch: Epoch,
        opened: bool,
    ) -> (ReplicaAgent, mpsc::Receiver<RuntimeEffectCommand>) {
        let (_command_tx, command_rx) = mpsc::channel(32);
        let (runtime_tx, runtime_rx) = mpsc::channel(32);
        let (_fault_tx, fault_rx) = mpsc::channel(16);
        let (_status_tx, status_rx) = watch::channel(RuntimeControlSnapshot {
            instance_id: ReplicaInstanceId::new("pod-uid"),
            role: if opened { Role::Primary } else { Role::Unknown },
            epoch,
            configuration: None,
            election_configuration: None,
            deactivation_info: None,
            partition_state: opened.then(|| Arc::new(PartitionState::new())),
        });
        (
            ReplicaAgent::new(
                1,
                ReplicaInstanceId::new("pod-uid"),
                command_rx,
                runtime_tx,
                status_rx,
                fault_rx,
                CancellationToken::new(),
            ),
            runtime_rx,
        )
    }

    fn terminal(action_id: &str) -> CorrelatedActionObservation {
        CorrelatedActionObservation {
            generation: AgentGeneration::from_string("generation"),
            control_version: AgentControlVersion::new(1),
            action: DurableActionObservation {
                action_id: action_id.to_string(),
                signature: DurableReplicaAction::Close.signature(),
                state: DurableActionState::Completed,
                error_class: None,
                error: None,
                result: None,
            },
        }
    }

    fn request(
        agent: &ReplicaAgent,
        action_id: &str,
        action: DurableReplicaAction,
    ) -> CorrelatedControlActionRequest {
        CorrelatedControlActionRequest {
            protocol_version: CORRELATED_CONTROL_PROTOCOL_VERSION,
            action_id: action_id.to_string(),
            input_signature: action.signature(),
            target_replica_id: agent.replica_id,
            target_instance_id: agent.instance_id.clone(),
            expected_agent_generation: agent.generation.clone(),
            expected_control_version: agent.control_version,
            observed_runtime_epoch: agent.runtime_status_rx.borrow().epoch,
            action,
        }
    }

    async fn accept(
        agent: &mut ReplicaAgent,
        request: CorrelatedControlActionRequest,
    ) -> Result<CorrelatedControlActionAcknowledgement> {
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_correlated(request, reply_tx);
        reply_rx.await.unwrap()
    }

    #[test]
    fn normalized_error_is_utf8_safe_and_bounded() {
        let normalized = normalize_error(&"é".repeat(MAX_ERROR_BYTES));
        assert!(normalized.len() <= MAX_ERROR_BYTES);
        assert!(normalized.is_char_boundary(normalized.len()));
        assert_eq!(
            normalize_error(&"x".repeat(MAX_ERROR_BYTES + 20)).len(),
            MAX_ERROR_BYTES
        );
    }

    #[test]
    fn agent_generations_are_process_local() {
        assert_ne!(AgentGeneration::generate(), AgentGeneration::generate());
    }

    #[tokio::test]
    async fn unsupported_protocol_and_all_fences_fail_before_effects() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::new(3, 4), true);

        let mut unsupported = request(
            &agent,
            "unsupported",
            DurableReplicaAction::RevokeWriteStatus,
        );
        unsupported.protocol_version += 1;
        assert!(matches!(
            accept(&mut agent, unsupported).await,
            Err(KubericError::UnsupportedControlProtocolVersion { .. })
        ));

        let mut target = request(&agent, "target", DurableReplicaAction::RevokeWriteStatus);
        target.target_instance_id = ReplicaInstanceId::new("replacement");
        assert!(matches!(
            accept(&mut agent, target).await,
            Err(KubericError::CorrelatedTargetMismatch { .. })
        ));

        let mut generation = request(
            &agent,
            "generation",
            DurableReplicaAction::RevokeWriteStatus,
        );
        generation.expected_agent_generation = AgentGeneration::generate();
        assert!(matches!(
            accept(&mut agent, generation).await,
            Err(KubericError::StaleAgentGeneration { .. })
        ));

        let mut epoch = request(&agent, "epoch", DurableReplicaAction::RevokeWriteStatus);
        epoch.observed_runtime_epoch = Epoch::new(3, 3);
        assert!(matches!(
            accept(&mut agent, epoch).await,
            Err(KubericError::StaleEpoch { .. })
        ));

        let mut version = request(&agent, "version", DurableReplicaAction::RevokeWriteStatus);
        version.expected_control_version = AgentControlVersion::new(1);
        assert!(matches!(
            accept(&mut agent, version).await,
            Err(KubericError::StaleAgentControlVersion { .. })
        ));
        assert_eq!(agent.control_version.value(), 0);
        assert!(runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn duplicate_replays_and_conflict_never_redispatches() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), true);
        let original = request(&agent, "action", DurableReplicaAction::RevokeWriteStatus);
        let duplicate = original.clone();
        let (first_tx, first_rx) = oneshot::channel();
        agent.accept_correlated(original, first_tx);
        let runtime = runtime_rx.recv().await.unwrap();

        let replay = accept(&mut agent, duplicate.clone()).await.unwrap();
        assert_eq!(
            replay.observation.action.state,
            DurableActionState::InProgress
        );
        assert!(runtime_rx.try_recv().is_err());

        let mut conflict = duplicate;
        conflict.input_signature = DurableReplicaAction::Close.signature();
        assert!(matches!(
            accept(&mut agent, conflict).await,
            Err(KubericError::ActionSignatureMismatch { .. })
        ));

        runtime.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let completion = agent.completion_rx.recv().await.unwrap();
        agent.handle_completion(completion);
        let completed = first_rx.await.unwrap().unwrap();
        assert_eq!(
            completed.observation.action.state,
            DurableActionState::Completed
        );

        let replay = request(&agent, "action", DurableReplicaAction::RevokeWriteStatus);
        let replay = accept(&mut agent, replay).await.unwrap();
        assert_eq!(
            replay.observation.action.state,
            DurableActionState::Completed
        );
        assert!(runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn reused_action_id_with_valid_different_signature_is_conflict() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), true);
        agent.retain_terminal(CorrelatedActionObservation {
            generation: agent.generation.clone(),
            control_version: AgentControlVersion::new(1),
            action: DurableActionObservation {
                action_id: "action".to_string(),
                signature: DurableReplicaAction::Close.signature(),
                state: DurableActionState::Completed,
                error_class: None,
                error: None,
                result: None,
            },
        });
        let request = request(&agent, "action", DurableReplicaAction::RevokeWriteStatus);
        assert!(matches!(
            accept(&mut agent, request).await,
            Err(KubericError::ActionIdConflict { .. })
        ));
        assert!(runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn data_loss_payload_epoch_mismatch_is_retained_failed_observation() {
        let runtime_epoch = Epoch::new(2, 7);
        let (mut agent, mut runtime_rx) = test_agent(runtime_epoch, true);
        let request = request(
            &agent,
            "data-loss",
            DurableReplicaAction::OnDataLoss {
                epoch: Epoch::new(1, 7),
            },
        );
        let acknowledgement = accept(&mut agent, request).await.unwrap();
        assert_eq!(
            acknowledgement.observation.action.state,
            DurableActionState::Failed
        );
        assert!(
            acknowledgement
                .observation
                .action
                .error
                .as_deref()
                .unwrap()
                .contains("does not match runtime epoch")
        );
        assert!(runtime_rx.try_recv().is_err());
        assert_eq!(agent.terminals.len(), 1);
    }

    #[tokio::test]
    async fn terminal_runtime_error_preserves_stable_classification() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), true);
        let request = request(
            &agent,
            "quorum-failure",
            DurableReplicaAction::WaitForCatchUpQuorum {
                mode: crate::types::ReplicaSetQuorumMode::Write,
            },
        );
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_correlated(request, reply_tx);
        let runtime = runtime_rx.recv().await.unwrap();
        runtime
            .reply
            .send(Err(KubericError::NoWriteQuorum))
            .unwrap();
        let completion = agent.completion_rx.recv().await.unwrap();
        agent.handle_completion(completion);

        let acknowledgement = reply_rx.await.unwrap().unwrap();
        assert_eq!(
            acknowledgement.observation.action.error_class,
            Some(DurableActionErrorClass::NoWriteQuorum)
        );
        assert_eq!(
            agent.terminals.back().unwrap().action.error_class,
            Some(DurableActionErrorClass::NoWriteQuorum)
        );
    }

    #[test]
    fn terminal_and_fault_retention_are_bounded_and_deterministic() {
        let (mut agent, _runtime_rx) = test_agent(Epoch::default(), false);
        for index in 0..=TERMINAL_RETENTION {
            agent.retain_terminal(terminal(&format!("action-{index}")));
            agent.record_fault(if index % 2 == 0 {
                FaultType::Transient
            } else {
                FaultType::Permanent
            });
        }
        assert_eq!(agent.terminals.len(), TERMINAL_RETENTION);
        assert_eq!(
            agent.terminals.front().unwrap().action.action_id,
            "action-1"
        );
        assert!(agent.find_correlated("action-0").is_none());
        assert_eq!(agent.faults.len(), FAULT_RETENTION);
        assert_eq!(agent.faults.front().unwrap().sequence, 2);
    }

    #[tokio::test]
    async fn unretained_old_version_fails_as_continuity_unavailable() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), true);
        agent.control_version = AgentControlVersion::new(1);
        let mut request = request(&agent, "evicted", DurableReplicaAction::RevokeWriteStatus);
        request.expected_control_version = AgentControlVersion::new(0);
        assert!(matches!(
            accept(&mut agent, request).await,
            Err(KubericError::CorrelatedContinuityUnavailable { .. })
        ));
        assert!(runtime_rx.try_recv().is_err());
    }

    #[test]
    fn status_is_required_generation_qualified_and_restart_local() {
        let (mut old_agent, _old_runtime) = test_agent(Epoch::default(), false);
        old_agent.retain_terminal(terminal("old-action"));
        let old_status = old_agent.status().agent;

        let (new_agent, _new_runtime) = test_agent(Epoch::default(), false);
        let new_status = new_agent.status().agent;
        assert_eq!(
            new_status.protocol_version,
            CORRELATED_CONTROL_PROTOCOL_VERSION
        );
        assert_ne!(old_status.generation, new_status.generation);
        assert_eq!(new_status.control_version.value(), 0);
        assert!(new_status.retained_terminal_actions.is_empty());
    }

    #[tokio::test]
    async fn same_pod_restart_rejects_prior_generation_after_ambiguous_completion() {
        let (mut old_agent, mut old_runtime_rx) = test_agent(Epoch::default(), true);
        let request = request(
            &old_agent,
            "ambiguous",
            DurableReplicaAction::RevokeWriteStatus,
        );
        let retry = request.clone();
        let (reply_tx, reply_rx) = oneshot::channel();
        old_agent.accept_correlated(request, reply_tx);
        drop(reply_rx);
        let runtime = old_runtime_rx.recv().await.unwrap();
        runtime.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let completion = old_agent.completion_rx.recv().await.unwrap();
        old_agent.handle_completion(completion);

        let (mut new_agent, mut new_runtime_rx) = test_agent(Epoch::default(), true);
        assert_eq!(new_agent.instance_id, old_agent.instance_id);
        assert_ne!(new_agent.generation, old_agent.generation);
        assert!(matches!(
            accept(&mut new_agent, retry).await,
            Err(KubericError::StaleAgentGeneration { .. })
        ));
        assert!(new_runtime_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn status_remains_available_while_effect_is_pending() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::new(1, 2), true);
        let request = request(&agent, "pending", DurableReplicaAction::RevokeWriteStatus);
        let (reply_tx, _reply_rx) = oneshot::channel();
        agent.accept_correlated(request, reply_tx);
        assert!(runtime_rx.recv().await.is_some());
        let status = agent.status();
        assert_eq!(status.epoch, Epoch::new(1, 2));
        assert_eq!(
            status.agent.current_action.unwrap().action.state,
            DurableActionState::InProgress
        );
    }

    #[tokio::test]
    async fn mismatched_and_late_completions_cannot_replace_active_state() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), true);
        let request = request(&agent, "active", DurableReplicaAction::RevokeWriteStatus);
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_correlated(request, reply_tx);
        let runtime = runtime_rx.recv().await.unwrap();
        let active_id = agent.active.as_ref().unwrap().execution_id.clone();
        agent.handle_completion(RuntimeCompletion {
            execution_id: RuntimeEffectExecutionId {
                generation: active_id.generation.clone(),
                sequence: active_id.sequence + 1,
            },
            result: Ok(RuntimeEffectResult::Unit),
        });
        assert!(agent.active.is_some());

        runtime.reply.send(Ok(RuntimeEffectResult::Unit)).unwrap();
        let completion = agent.completion_rx.recv().await.unwrap();
        agent.handle_completion(completion);
        assert_eq!(
            reply_rx.await.unwrap().unwrap().observation.action.state,
            DurableActionState::Completed
        );
        assert!(agent.active.is_none());

        agent.handle_completion(RuntimeCompletion {
            execution_id: active_id,
            result: Ok(RuntimeEffectResult::Unit),
        });
        assert!(agent.active.is_none());
    }
}
