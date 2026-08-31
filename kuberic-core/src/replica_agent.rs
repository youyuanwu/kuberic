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
    CorrelatedActionAdmission, CorrelatedActionObservation, CorrelatedControlActionAcknowledgement,
    CorrelatedControlActionRequest, DataLossAction, DurableActionCompletion,
    DurableActionObservation, DurableActionResult, DurableActionState, DurableReplicaAction, Epoch,
    FaultType, LocalFaultRecord, OpenMode, ReplicaAgentCapability, ReplicaAgentStatus, ReplicaId,
    ReplicaInfo, ReplicaInstanceId, ReplicaSetConfig, ReplicaSetQuorumMode, ReplicaStatusInfo,
    Role,
};

const TERMINAL_RETENTION: usize = 16;
const FAULT_RETENTION: usize = 16;
const DIRECT_QUEUE_CAPACITY: usize = 16;
const MAX_ERROR_BYTES: usize = 1024;
pub const CORRELATED_CONTROL_PROTOCOL_VERSION: u32 = 1;

/// Transport-facing commands accepted by the pod-local replica agent.
pub enum AgentCommand {
    Open {
        mode: OpenMode,
        reply: oneshot::Sender<Result<()>>,
    },
    Close {
        reply: oneshot::Sender<Result<()>>,
    },
    ChangeRole {
        epoch: Epoch,
        role: Role,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateEpoch {
        epoch: Epoch,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateCatchUpConfiguration {
        current: ReplicaSetConfig,
        previous: ReplicaSetConfig,
        reply: oneshot::Sender<Result<()>>,
    },
    UpdateCurrentConfiguration {
        current: ReplicaSetConfig,
        reply: oneshot::Sender<Result<()>>,
    },
    WaitForCatchUpQuorum {
        mode: ReplicaSetQuorumMode,
        reply: oneshot::Sender<Result<()>>,
    },
    BuildReplica {
        replica: ReplicaInfo,
        reply: oneshot::Sender<Result<()>>,
    },
    RemoveReplica {
        replica_id: ReplicaId,
        instance_id: ReplicaInstanceId,
        reply: oneshot::Sender<Result<()>>,
    },
    OnDataLoss {
        reply: oneshot::Sender<Result<DataLossAction>>,
    },
    RevokeWriteStatus {
        reply: oneshot::Sender<Result<()>>,
    },
    ExecuteDurableAction {
        action_id: String,
        action: DurableReplicaAction,
        reply: oneshot::Sender<Result<()>>,
    },
    ExecuteCorrelatedControlAction {
        request: CorrelatedControlActionRequest,
        reply: oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>,
    },
    GetStatus {
        reply: oneshot::Sender<ReplicaStatusInfo>,
    },
}

enum ClientReply {
    Unit(oneshot::Sender<Result<()>>),
    DataLoss(oneshot::Sender<Result<DataLossAction>>),
}

enum CorrelatedClientReply {
    Legacy(oneshot::Sender<Result<()>>),
    Versioned(oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>),
}

impl CorrelatedClientReply {
    fn replay(self, observation: &CorrelatedActionObservation) {
        match self {
            Self::Legacy(reply) => {
                let result = match observation.action.state {
                    DurableActionState::Scheduled
                    | DurableActionState::InProgress
                    | DurableActionState::Completed => Ok(()),
                    DurableActionState::Failed => Err(KubericError::Internal(
                        observation
                            .action
                            .error
                            .clone()
                            .unwrap_or_else(|| "durable action failed".to_string())
                            .into(),
                    )),
                };
                let _ = reply.send(result);
            }
            Self::Versioned(reply) => {
                let _ = reply.send(Ok(CorrelatedControlActionAcknowledgement {
                    observation: observation.clone(),
                }));
            }
        }
    }

    fn accepted(self, observation: &CorrelatedActionObservation) {
        match self {
            Self::Legacy(reply) => {
                let _ = reply.send(Ok(()));
            }
            Self::Versioned(reply) => {
                let _ = reply.send(Ok(CorrelatedControlActionAcknowledgement {
                    observation: observation.clone(),
                }));
            }
        }
    }

    fn terminal(self, observation: &CorrelatedActionObservation, legacy_result: Result<()>) {
        match self {
            Self::Legacy(reply) => {
                let bounded_result = legacy_result.map_err(|error| {
                    let message = error.to_string();
                    if message.len() > MAX_ERROR_BYTES {
                        KubericError::Internal(normalize_error(&message).into())
                    } else {
                        error
                    }
                });
                let _ = reply.send(bounded_result);
            }
            Self::Versioned(reply) => {
                let _ = reply.send(Ok(CorrelatedControlActionAcknowledgement {
                    observation: observation.clone(),
                }));
            }
        }
    }

    fn reject(self, error: KubericError) {
        match self {
            Self::Legacy(reply) => {
                let _ = reply.send(Err(error));
            }
            Self::Versioned(reply) => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

impl ClientReply {
    fn send(self, result: Result<RuntimeEffectResult>) {
        match self {
            Self::Unit(reply) => {
                let result = result.and_then(|value| match value {
                    RuntimeEffectResult::Unit => Ok(()),
                    RuntimeEffectResult::DataLoss(_) => Err(KubericError::Internal(
                        "runtime returned data-loss result for unit effect".into(),
                    )),
                });
                let _ = reply.send(result);
            }
            Self::DataLoss(reply) => {
                let result = result.and_then(|value| match value {
                    RuntimeEffectResult::DataLoss(action) => Ok(action),
                    RuntimeEffectResult::Unit => Err(KubericError::Internal(
                        "runtime returned unit result for data-loss effect".into(),
                    )),
                });
                let _ = reply.send(result);
            }
        }
    }

    fn reject(self, error: KubericError) {
        match self {
            Self::Unit(reply) => {
                let _ = reply.send(Err(error));
            }
            Self::DataLoss(reply) => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

struct PendingDirect {
    effect: RuntimeEffect,
    reply: ClientReply,
    close_barrier: bool,
}

struct ActiveDirect {
    execution_id: RuntimeEffectExecutionId,
    reply: ClientReply,
    close_barrier: bool,
}

struct ActiveCorrelated {
    execution_id: RuntimeEffectExecutionId,
    observation: CorrelatedActionObservation,
    reply: Option<CorrelatedClientReply>,
}

enum ActiveMutation {
    Direct(ActiveDirect),
    Correlated(ActiveCorrelated),
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

/// Pod-local control owner between transport and ordered runtime effects.
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
    active: Option<ActiveMutation>,
    direct_queue: VecDeque<PendingDirect>,
    terminals: VecDeque<CorrelatedActionObservation>,
    faults: VecDeque<LocalFaultRecord>,
    close_queued: bool,
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
            direct_queue: VecDeque::with_capacity(DIRECT_QUEUE_CAPACITY),
            terminals: VecDeque::with_capacity(TERMINAL_RETENTION),
            faults: VecDeque::with_capacity(FAULT_RETENTION),
            close_queued: false,
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
            if self.shutting_down && self.active.is_none() && self.direct_queue.is_empty() {
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
                        Some(command) => self.handle_command(command),
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

    fn handle_command(&mut self, command: AgentCommand) {
        match command {
            AgentCommand::GetStatus { reply } => {
                let _ = reply.send(self.status());
            }
            AgentCommand::ExecuteDurableAction {
                action_id,
                action,
                reply,
            } => self.accept_legacy_correlated(action_id, action, reply),
            AgentCommand::ExecuteCorrelatedControlAction { request, reply } => {
                self.accept_versioned_correlated(request, reply)
            }
            AgentCommand::Open { mode, reply } => self.accept_direct(PendingDirect {
                effect: RuntimeEffect::Open { mode },
                reply: ClientReply::Unit(reply),
                close_barrier: false,
            }),
            AgentCommand::Close { reply } => self.accept_direct(PendingDirect {
                effect: RuntimeEffect::Close {
                    terminate_runtime: true,
                },
                reply: ClientReply::Unit(reply),
                close_barrier: true,
            }),
            AgentCommand::ChangeRole { epoch, role, reply } => self.accept_direct(PendingDirect {
                effect: RuntimeEffect::ChangeRole { epoch, role },
                reply: ClientReply::Unit(reply),
                close_barrier: false,
            }),
            AgentCommand::UpdateEpoch { epoch, reply } => self.accept_direct(PendingDirect {
                effect: RuntimeEffect::UpdateEpoch { epoch },
                reply: ClientReply::Unit(reply),
                close_barrier: false,
            }),
            AgentCommand::UpdateCatchUpConfiguration {
                current,
                previous,
                reply,
            } => self.accept_direct(PendingDirect {
                effect: RuntimeEffect::UpdateCatchUpConfiguration {
                    current,
                    previous,
                    observe_on_secondary: false,
                },
                reply: ClientReply::Unit(reply),
                close_barrier: false,
            }),
            AgentCommand::UpdateCurrentConfiguration { current, reply } => {
                self.accept_direct(PendingDirect {
                    effect: RuntimeEffect::UpdateCurrentConfiguration {
                        current,
                        observe_on_secondary: false,
                    },
                    reply: ClientReply::Unit(reply),
                    close_barrier: false,
                })
            }
            AgentCommand::WaitForCatchUpQuorum { mode, reply } => {
                self.accept_direct(PendingDirect {
                    effect: RuntimeEffect::WaitForCatchUpQuorum { mode },
                    reply: ClientReply::Unit(reply),
                    close_barrier: false,
                })
            }
            AgentCommand::BuildReplica { replica, reply } => self.accept_direct(PendingDirect {
                effect: RuntimeEffect::BuildReplica {
                    replica,
                    extended_timeout: false,
                },
                reply: ClientReply::Unit(reply),
                close_barrier: false,
            }),
            AgentCommand::RemoveReplica {
                replica_id,
                instance_id,
                reply,
            } => self.accept_direct(PendingDirect {
                effect: RuntimeEffect::RemoveReplica {
                    replica_id,
                    instance_id,
                },
                reply: ClientReply::Unit(reply),
                close_barrier: false,
            }),
            AgentCommand::OnDataLoss { reply } => self.accept_direct(PendingDirect {
                effect: RuntimeEffect::OnDataLoss {
                    expected_epoch: None,
                },
                reply: ClientReply::DataLoss(reply),
                close_barrier: false,
            }),
            AgentCommand::RevokeWriteStatus { reply } => self.accept_direct(PendingDirect {
                effect: RuntimeEffect::RevokeWriteStatus {
                    require_open: false,
                },
                reply: ClientReply::Unit(reply),
                close_barrier: false,
            }),
        }
    }

    fn accept_direct(&mut self, pending: PendingDirect) {
        if self.close_queued || self.shutting_down {
            pending.reply.reject(KubericError::Closed);
            return;
        }
        if matches!(self.active, Some(ActiveMutation::Correlated(_))) {
            warn!(
                replica_id = self.replica_id,
                "rejecting direct mutation while correlated action is active"
            );
            pending.reply.reject(KubericError::AgentBusy);
            return;
        }

        if self.active.is_some() {
            if self.direct_queue.len() >= DIRECT_QUEUE_CAPACITY {
                warn!(
                    replica_id = self.replica_id,
                    capacity = DIRECT_QUEUE_CAPACITY,
                    "rejecting direct mutation because agent queue is full"
                );
                pending.reply.reject(KubericError::AgentQueueFull);
                return;
            }
            self.control_version.advance();
            self.close_queued |= pending.close_barrier;
            self.direct_queue.push_back(pending);
            return;
        }

        self.control_version.advance();
        self.close_queued |= pending.close_barrier;
        self.start_direct(pending);
    }

    fn start_direct(&mut self, pending: PendingDirect) {
        let execution_id = self.next_execution_id();
        let close_barrier = pending.close_barrier;
        self.dispatch_runtime(execution_id.clone(), pending.effect);
        self.active = Some(ActiveMutation::Direct(ActiveDirect {
            execution_id,
            reply: pending.reply,
            close_barrier,
        }));
    }

    fn accept_versioned_correlated(
        &mut self,
        request: CorrelatedControlActionRequest,
        reply: oneshot::Sender<Result<CorrelatedControlActionAcknowledgement>>,
    ) {
        let reply = CorrelatedClientReply::Versioned(reply);
        if request.protocol_version != CORRELATED_CONTROL_PROTOCOL_VERSION {
            reply.reject(KubericError::UnsupportedControlProtocolVersion {
                got: request.protocol_version,
            });
            return;
        }
        if request.target_replica_id != self.replica_id
            || request.target_instance_id != self.instance_id
        {
            reply.reject(KubericError::CorrelatedTargetMismatch {
                expected_id: self.replica_id,
                expected_instance: self.instance_id.clone(),
                actual_id: request.target_replica_id,
                actual_instance: request.target_instance_id,
            });
            return;
        }
        if request.expected_agent_generation != self.generation {
            reply.reject(KubericError::StaleAgentGeneration {
                expected: request.expected_agent_generation,
                current: self.generation.clone(),
            });
            return;
        }
        let actual_signature = request.action.signature();
        if request.input_signature != actual_signature {
            reply.reject(KubericError::ActionSignatureMismatch {
                action_id: request.action_id,
            });
            return;
        }
        if let Some(observed) = self.find_correlated(&request.action_id) {
            if observed.action.signature != request.input_signature {
                reply.reject(KubericError::ActionIdConflict {
                    action_id: request.action_id,
                });
            } else {
                reply.replay(observed);
            }
            return;
        }
        if request.expected_control_version != self.control_version {
            if request.expected_control_version < self.control_version {
                reply.reject(KubericError::CorrelatedContinuityUnavailable {
                    action_id: request.action_id,
                });
            } else {
                reply.reject(KubericError::StaleAgentControlVersion {
                    expected: request.expected_control_version,
                    current: self.control_version,
                });
            }
            return;
        }
        if self.active.is_some() || !self.direct_queue.is_empty() {
            reply.reject(KubericError::AgentBusy);
            return;
        }
        let runtime_epoch = self.runtime_status_rx.borrow().epoch;
        if request.observed_runtime_epoch != runtime_epoch {
            reply.reject(KubericError::StaleEpoch {
                got: request.observed_runtime_epoch,
                current: runtime_epoch,
            });
            return;
        }
        self.accept_correlated(
            request.action_id,
            request.action,
            CorrelatedActionAdmission::Versioned,
            Some(request.observed_runtime_epoch),
            reply,
        );
    }

    fn accept_legacy_correlated(
        &mut self,
        action_id: String,
        action: DurableReplicaAction,
        reply: oneshot::Sender<Result<()>>,
    ) {
        self.accept_correlated(
            action_id,
            action,
            CorrelatedActionAdmission::Legacy,
            None,
            CorrelatedClientReply::Legacy(reply),
        );
    }

    fn accept_correlated(
        &mut self,
        action_id: String,
        action: DurableReplicaAction,
        admission: CorrelatedActionAdmission,
        expected_runtime_epoch: Option<Epoch>,
        reply: CorrelatedClientReply,
    ) {
        if action_id.is_empty() {
            reply.reject(KubericError::InvalidCorrelatedActionId);
            return;
        }
        if self.close_queued || self.shutting_down {
            reply.reject(KubericError::Closed);
            return;
        }
        let signature = action.signature();
        if let Some(observed) = self.find_correlated(&action_id) {
            if observed.action.signature != signature {
                warn!(
                    action_id,
                    agent_generation = %self.generation,
                    "rejecting correlated action ID reused with different input"
                );
                reply.reject(KubericError::ActionIdConflict { action_id });
                return;
            }
            debug!(
                action_id,
                state = ?observed.action.state,
                agent_generation = %self.generation,
                "replaying correlated action observation"
            );
            reply.replay(observed);
            return;
        }

        if self.active.is_some() || !self.direct_queue.is_empty() {
            warn!(
                action_id,
                agent_generation = %self.generation,
                "rejecting correlated action while another mutation is active"
            );
            reply.reject(KubericError::AgentBusy);
            return;
        }
        if let Some(expected) = expected_runtime_epoch {
            let current = self.runtime_status_rx.borrow().epoch;
            if current != expected {
                reply.reject(KubericError::StaleEpoch {
                    got: expected,
                    current,
                });
                return;
            }
        }

        let control_version = self.control_version.advance();
        let observation = CorrelatedActionObservation {
            generation: self.generation.clone(),
            control_version,
            admission,
            action: DurableActionObservation {
                action_id: action_id.clone(),
                signature,
                state: DurableActionState::Scheduled,
                error: None,
                result: None,
            },
        };
        debug!(
            action_id,
            control_version = control_version.value(),
            agent_generation = %self.generation,
            ?admission,
            "accepted correlated action"
        );

        if let DurableReplicaAction::OnDataLoss { epoch } = action {
            let runtime_epoch = self.runtime_status_rx.borrow().epoch;
            if runtime_epoch != epoch {
                let error = KubericError::Internal(
                    format!(
                        "data-loss action epoch {:?} does not match runtime epoch {:?}",
                        epoch, runtime_epoch
                    )
                    .into(),
                );
                let mut observation = observation;
                observation.action.state = DurableActionState::Failed;
                observation.action.error = Some(normalize_error(&error.to_string()));
                reply.terminal(&observation, Err(error));
                self.retain_terminal(observation);
                warn!(
                    action_id,
                    expected_epoch = ?epoch,
                    runtime_epoch = ?runtime_epoch,
                    "rejecting durable data-loss action at mismatched epoch"
                );
                return;
            }
            if self.runtime_status_rx.borrow().partition_state.is_none() {
                self.fail_before_dispatch(observation, reply, "replicator not opened");
                return;
            }
            let execution_id = self.next_execution_id();
            let mut observation = observation;
            observation.action.state = DurableActionState::InProgress;
            reply.accepted(&observation);
            self.dispatch_runtime(
                execution_id.clone(),
                RuntimeEffect::OnDataLoss {
                    expected_epoch: Some(epoch),
                },
            );
            self.active = Some(ActiveMutation::Correlated(ActiveCorrelated {
                execution_id,
                observation,
                reply: None,
            }));
            return;
        }

        if let DurableReplicaAction::BuildReplica { replica } = action {
            if self.runtime_status_rx.borrow().partition_state.is_none() {
                self.fail_before_dispatch(observation, reply, "replicator not opened");
                return;
            }
            let execution_id = self.next_execution_id();
            let mut observation = observation;
            observation.action.state = DurableActionState::InProgress;
            reply.accepted(&observation);
            self.dispatch_runtime(
                execution_id.clone(),
                RuntimeEffect::BuildReplica {
                    replica,
                    extended_timeout: true,
                },
            );
            self.active = Some(ActiveMutation::Correlated(ActiveCorrelated {
                execution_id,
                observation,
                reply: None,
            }));
            return;
        }

        let effect = correlated_runtime_effect(action);
        let execution_id = self.next_execution_id();
        let mut observation = observation;
        observation.action.state = DurableActionState::InProgress;
        self.dispatch_runtime(execution_id.clone(), effect);
        self.active = Some(ActiveMutation::Correlated(ActiveCorrelated {
            execution_id,
            observation,
            reply: Some(reply),
        }));
    }

    fn fail_before_dispatch(
        &mut self,
        mut observation: CorrelatedActionObservation,
        reply: CorrelatedClientReply,
        message: &'static str,
    ) {
        observation.action.state = DurableActionState::Failed;
        observation.action.error = Some(message.to_string());
        reply.terminal(&observation, Err(KubericError::Internal(message.into())));
        self.retain_terminal(observation);
    }

    fn dispatch_runtime(&mut self, execution_id: RuntimeEffectExecutionId, effect: RuntimeEffect) {
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
        let Some(active) = self.active.take() else {
            warn!(
                execution_sequence = completion.execution_id.sequence,
                "discarding runtime completion without active mutation"
            );
            return;
        };
        match active {
            ActiveMutation::Direct(active) => {
                if active.execution_id != completion.execution_id {
                    warn!("discarding mismatched direct runtime completion");
                    self.active = Some(ActiveMutation::Direct(active));
                    return;
                }
                active.reply.send(completion.result);
                if active.close_barrier {
                    self.shutting_down = true;
                    self.reject_queued();
                } else {
                    self.start_next_direct();
                }
            }
            ActiveMutation::Correlated(mut active) => {
                if active.execution_id != completion.execution_id {
                    warn!("discarding mismatched correlated runtime completion");
                    self.active = Some(ActiveMutation::Correlated(active));
                    return;
                }
                let client_result = match completion.result {
                    Ok(RuntimeEffectResult::Unit) => {
                        active.observation.action.state = DurableActionState::Completed;
                        Ok(())
                    }
                    Ok(RuntimeEffectResult::DataLoss(result)) => {
                        active.observation.action.state = DurableActionState::Completed;
                        active.observation.action.result =
                            Some(DurableActionResult::DataLoss(result));
                        Ok(())
                    }
                    Err(error) => {
                        active.observation.action.state = DurableActionState::Failed;
                        active.observation.action.error = Some(normalize_error(&error.to_string()));
                        Err(error)
                    }
                };
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
                    reply.terminal(&active.observation, client_result);
                }
                self.retain_terminal(active.observation);
                self.start_next_direct();
            }
        }
    }

    fn start_next_direct(&mut self) {
        if self.active.is_none()
            && !self.shutting_down
            && let Some(pending) = self.direct_queue.pop_front()
        {
            self.start_direct(pending);
        }
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
        self.reject_queued();
    }

    fn reject_queued(&mut self) {
        for pending in self.direct_queue.drain(..) {
            pending.reply.reject(KubericError::Closed);
        }
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
        let projected = match self.active.as_ref() {
            Some(ActiveMutation::Correlated(active)) => Some(active.observation.action.clone()),
            _ => self.terminals.back().map(|entry| entry.action.clone()),
        };
        let last_completed_action = projected.as_ref().and_then(|action| {
            (action.state == DurableActionState::Completed).then(|| DurableActionCompletion {
                action_id: action.action_id.clone(),
                signature: action.signature.clone(),
                result: action.result,
            })
        });
        let current_action = match self.active.as_ref() {
            Some(ActiveMutation::Correlated(active)) => Some(active.observation.clone()),
            _ => None,
        };
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
            last_completed_action,
            durable_action: projected,
            active_replica_connections: state
                .map_or_else(Vec::new, |state| state.active_replica_connections()),
            agent: Some(ReplicaAgentStatus {
                generation: self.generation.clone(),
                control_version: self.control_version,
                capabilities: vec![ReplicaAgentCapability::CorrelatedControlActionV1],
                current_action,
                retained_terminal_actions: self.terminals.iter().cloned().collect(),
                local_faults: self.faults.iter().copied().collect(),
            }),
        }
    }

    fn find_correlated(&self, action_id: &str) -> Option<&CorrelatedActionObservation> {
        match self.active.as_ref() {
            Some(ActiveMutation::Correlated(active))
                if active.observation.action.action_id == action_id =>
            {
                Some(&active.observation)
            }
            _ => self
                .terminals
                .iter()
                .rev()
                .find(|entry| entry.action.action_id == action_id),
        }
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
        DurableReplicaAction::Close => RuntimeEffect::Close {
            terminate_runtime: false,
        },
        DurableReplicaAction::RevokeWriteStatus => {
            RuntimeEffect::RevokeWriteStatus { require_open: true }
        }
        DurableReplicaAction::ChangeRole { epoch, role } => {
            RuntimeEffect::ChangeRole { epoch, role }
        }
        DurableReplicaAction::UpdateEpoch { epoch } => RuntimeEffect::UpdateEpoch { epoch },
        DurableReplicaAction::UpdateCatchUpConfiguration { current, previous } => {
            RuntimeEffect::UpdateCatchUpConfiguration {
                current,
                previous,
                observe_on_secondary: true,
            }
        }
        DurableReplicaAction::WaitForCatchUpQuorum { mode } => {
            RuntimeEffect::WaitForCatchUpQuorum { mode }
        }
        DurableReplicaAction::UpdateCurrentConfiguration { current } => {
            RuntimeEffect::UpdateCurrentConfiguration {
                current,
                observe_on_secondary: true,
            }
        }
        DurableReplicaAction::BuildReplica { replica } => RuntimeEffect::BuildReplica {
            replica,
            extended_timeout: true,
        },
        DurableReplicaAction::RemoveReplica {
            replica_id,
            instance_id,
        } => RuntimeEffect::RemoveReplica {
            replica_id,
            instance_id,
        },
        DurableReplicaAction::OnDataLoss { epoch } => RuntimeEffect::OnDataLoss {
            expected_epoch: Some(epoch),
        },
        DurableReplicaAction::RecordElectionConfiguration { configuration } => {
            RuntimeEffect::RecordElectionConfiguration { configuration }
        }
    }
}

fn reject_agent_command(command: AgentCommand, error: KubericError) {
    match command {
        AgentCommand::Open { reply, .. }
        | AgentCommand::Close { reply }
        | AgentCommand::ChangeRole { reply, .. }
        | AgentCommand::UpdateEpoch { reply, .. }
        | AgentCommand::UpdateCatchUpConfiguration { reply, .. }
        | AgentCommand::UpdateCurrentConfiguration { reply, .. }
        | AgentCommand::WaitForCatchUpQuorum { reply, .. }
        | AgentCommand::BuildReplica { reply, .. }
        | AgentCommand::RemoveReplica { reply, .. }
        | AgentCommand::RevokeWriteStatus { reply }
        | AgentCommand::ExecuteDurableAction { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        AgentCommand::OnDataLoss { reply } => {
            let _ = reply.send(Err(error));
        }
        AgentCommand::ExecuteCorrelatedControlAction { reply, .. } => {
            let _ = reply.send(Err(error));
        }
        AgentCommand::GetStatus { .. } => {}
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::Mutex;

    use crate::handles::PartitionState;

    #[derive(Clone, Default)]
    struct CapturedWriter(Arc<Mutex<Vec<u8>>>);

    struct CaptureHandle(Arc<Mutex<Vec<u8>>>);

    impl Write for CaptureHandle {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturedWriter {
        type Writer = CaptureHandle;

        fn make_writer(&'a self) -> Self::Writer {
            CaptureHandle(self.0.clone())
        }
    }

    impl CapturedWriter {
        fn contents(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

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
            admission: CorrelatedActionAdmission::Legacy,
            action: DurableActionObservation {
                action_id: action_id.to_string(),
                signature: "signature".to_string(),
                state: DurableActionState::Completed,
                error: None,
                result: None,
            },
        }
    }

    fn versioned_request(
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

    #[test]
    fn normalized_error_is_utf8_safe_and_bounded() {
        let error = "é".repeat(MAX_ERROR_BYTES);
        let normalized = normalize_error(&error);
        assert!(normalized.len() <= MAX_ERROR_BYTES);
        assert!(normalized.is_char_boundary(normalized.len()));
    }

    #[test]
    fn agent_generations_are_process_local() {
        assert_ne!(AgentGeneration::generate(), AgentGeneration::generate());
    }

    #[tokio::test]
    async fn legacy_duplicates_share_one_runtime_execution_and_replay_completion() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), false);
        let action = DurableReplicaAction::RevokeWriteStatus;
        let (first_tx, first_rx) = oneshot::channel();
        agent.accept_legacy_correlated("action-1".to_string(), action.clone(), first_tx);

        let runtime = runtime_rx.recv().await.unwrap();
        let (duplicate_tx, duplicate_rx) = oneshot::channel();
        agent.accept_legacy_correlated("action-1".to_string(), action.clone(), duplicate_tx);
        duplicate_rx.await.unwrap().unwrap();
        let (direct_tx, direct_rx) = oneshot::channel();
        agent.accept_direct(PendingDirect {
            effect: RuntimeEffect::UpdateEpoch {
                epoch: Epoch::new(1, 1),
            },
            reply: ClientReply::Unit(direct_tx),
            close_barrier: false,
        });
        assert!(matches!(
            direct_rx.await.unwrap(),
            Err(KubericError::AgentBusy)
        ));
        assert!(runtime_rx.try_recv().is_err());

        assert!(runtime.reply.send(Ok(RuntimeEffectResult::Unit)).is_ok());
        let completion = agent.completion_rx.recv().await.unwrap();
        agent.handle_completion(completion);
        first_rx.await.unwrap().unwrap();

        let (replay_tx, replay_rx) = oneshot::channel();
        agent.accept_legacy_correlated("action-1".to_string(), action, replay_tx);
        replay_rx.await.unwrap().unwrap();
        assert!(runtime_rx.try_recv().is_err());
        assert_eq!(agent.terminals.len(), 1);
    }

    #[tokio::test]
    async fn retained_action_id_conflict_is_rejected_without_runtime_effect() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), false);
        agent.retain_terminal(CorrelatedActionObservation {
            generation: agent.generation.clone(),
            control_version: AgentControlVersion::new(1),
            admission: CorrelatedActionAdmission::Legacy,
            action: DurableActionObservation {
                action_id: "action-1".to_string(),
                signature: DurableReplicaAction::Close.signature(),
                state: DurableActionState::Completed,
                error: None,
                result: None,
            },
        });

        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_legacy_correlated(
            "action-1".to_string(),
            DurableReplicaAction::RevokeWriteStatus,
            reply_tx,
        );
        assert!(matches!(
            reply_rx.await.unwrap(),
            Err(KubericError::ActionIdConflict { .. })
        ));
        assert!(runtime_rx.try_recv().is_err());
        assert_eq!(agent.control_version.value(), 0);
    }

    #[tokio::test]
    async fn data_loss_epoch_mismatch_is_a_retained_failure_without_effect() {
        let epoch = Epoch::new(2, 7);
        let (mut agent, mut runtime_rx) = test_agent(epoch, true);
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_legacy_correlated(
            "data-loss".to_string(),
            DurableReplicaAction::OnDataLoss {
                epoch: Epoch::new(1, 7),
            },
            reply_tx,
        );

        let error = reply_rx.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("does not match runtime epoch"));
        assert!(runtime_rx.try_recv().is_err());
        assert_eq!(agent.control_version.value(), 1);
        let retained = agent.terminals.back().unwrap();
        assert_eq!(retained.action.state, DurableActionState::Failed);
        assert!(
            retained
                .action
                .error
                .as_deref()
                .unwrap()
                .contains("does not match runtime epoch")
        );
    }

    #[tokio::test]
    async fn versioned_data_loss_mismatch_returns_a_definitive_failed_acknowledgement() {
        let runtime_epoch = Epoch::new(2, 7);
        let (mut agent, mut runtime_rx) = test_agent(runtime_epoch, true);
        let mut request = versioned_request(
            &agent,
            "data-loss",
            DurableReplicaAction::OnDataLoss {
                epoch: Epoch::new(1, 7),
            },
        );
        request.observed_runtime_epoch = runtime_epoch;
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_versioned_correlated(request, reply_tx);

        let acknowledgement = reply_rx.await.unwrap().unwrap();
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
    async fn empty_legacy_action_id_is_rejected_without_retention() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), false);
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_legacy_correlated(
            String::new(),
            DurableReplicaAction::RevokeWriteStatus,
            reply_tx,
        );
        assert!(matches!(
            reply_rx.await.unwrap(),
            Err(KubericError::InvalidCorrelatedActionId)
        ));
        assert!(runtime_rx.try_recv().is_err());
        assert!(agent.terminals.is_empty());
        assert_eq!(agent.control_version.value(), 0);
    }

    #[test]
    fn terminal_and_fault_retention_evict_the_oldest_entry() {
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
    async fn direct_queue_is_bounded_and_correlated_work_is_symmetric_busy() {
        let (mut agent, _runtime_rx) = test_agent(Epoch::default(), false);
        let (first_tx, _first_rx) = oneshot::channel();
        agent.accept_direct(PendingDirect {
            effect: RuntimeEffect::Open {
                mode: OpenMode::New,
            },
            reply: ClientReply::Unit(first_tx),
            close_barrier: false,
        });
        for _ in 0..DIRECT_QUEUE_CAPACITY {
            let (reply_tx, _reply_rx) = oneshot::channel();
            agent.accept_direct(PendingDirect {
                effect: RuntimeEffect::UpdateEpoch {
                    epoch: Epoch::default(),
                },
                reply: ClientReply::Unit(reply_tx),
                close_barrier: false,
            });
        }
        assert_eq!(agent.direct_queue.len(), DIRECT_QUEUE_CAPACITY);
        assert_eq!(
            agent.control_version.value(),
            (DIRECT_QUEUE_CAPACITY + 1) as u64
        );

        let (overflow_tx, overflow_rx) = oneshot::channel();
        agent.accept_direct(PendingDirect {
            effect: RuntimeEffect::UpdateEpoch {
                epoch: Epoch::default(),
            },
            reply: ClientReply::Unit(overflow_tx),
            close_barrier: false,
        });
        assert!(matches!(
            overflow_rx.await.unwrap(),
            Err(KubericError::AgentQueueFull)
        ));
        assert_eq!(
            agent.control_version.value(),
            (DIRECT_QUEUE_CAPACITY + 1) as u64
        );

        let (correlated_tx, correlated_rx) = oneshot::channel();
        agent.accept_legacy_correlated(
            "correlated".to_string(),
            DurableReplicaAction::Close,
            correlated_tx,
        );
        assert!(matches!(
            correlated_rx.await.unwrap(),
            Err(KubericError::AgentBusy)
        ));
    }

    #[test]
    fn status_is_generation_qualified_and_does_not_inherit_restart_state() {
        let (mut old_agent, _old_runtime) = test_agent(Epoch::default(), false);
        old_agent.retain_terminal(terminal("old-action"));
        let old_status = old_agent.status().agent.unwrap();

        let (new_agent, _new_runtime) = test_agent(Epoch::default(), false);
        let new_status = new_agent.status().agent.unwrap();
        assert_ne!(old_status.generation, new_status.generation);
        assert_eq!(new_status.control_version.value(), 0);
        assert!(new_status.retained_terminal_actions.is_empty());
    }

    #[tokio::test]
    async fn status_remains_available_while_runtime_effect_is_pending() {
        let (mut agent, _runtime_rx) = test_agent(Epoch::new(1, 2), true);
        let (reply_tx, _reply_rx) = oneshot::channel();
        agent.accept_legacy_correlated(
            "pending".to_string(),
            DurableReplicaAction::RevokeWriteStatus,
            reply_tx,
        );
        let status = agent.status();
        assert_eq!(status.epoch, Epoch::new(1, 2));
        assert_eq!(
            status.durable_action.unwrap().state,
            DurableActionState::InProgress
        );
    }

    #[tokio::test]
    async fn versioned_admission_fails_closed_before_runtime_effects() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::new(3, 4), true);

        let mut request =
            versioned_request(&agent, "versioned", DurableReplicaAction::RevokeWriteStatus);
        request.protocol_version += 1;
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_versioned_correlated(request, reply_tx);
        assert!(matches!(
            reply_rx.await.unwrap(),
            Err(KubericError::UnsupportedControlProtocolVersion { .. })
        ));

        let mut request =
            versioned_request(&agent, "versioned", DurableReplicaAction::RevokeWriteStatus);
        request.target_instance_id = ReplicaInstanceId::new("replacement-pod");
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_versioned_correlated(request, reply_tx);
        assert!(matches!(
            reply_rx.await.unwrap(),
            Err(KubericError::CorrelatedTargetMismatch { .. })
        ));

        let mut request =
            versioned_request(&agent, "versioned", DurableReplicaAction::RevokeWriteStatus);
        request.expected_agent_generation = AgentGeneration::generate();
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_versioned_correlated(request, reply_tx);
        assert!(matches!(
            reply_rx.await.unwrap(),
            Err(KubericError::StaleAgentGeneration { .. })
        ));

        let mut request =
            versioned_request(&agent, "versioned", DurableReplicaAction::RevokeWriteStatus);
        request.observed_runtime_epoch = Epoch::new(3, 3);
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_versioned_correlated(request, reply_tx);
        assert!(matches!(
            reply_rx.await.unwrap(),
            Err(KubericError::StaleEpoch { .. })
        ));
        assert_eq!(agent.control_version.value(), 0);
        assert!(runtime_rx.try_recv().is_err());

        let mut request =
            versioned_request(&agent, "versioned", DurableReplicaAction::RevokeWriteStatus);
        request.expected_control_version = AgentControlVersion::new(1);
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_versioned_correlated(request, reply_tx);
        assert!(matches!(
            reply_rx.await.unwrap(),
            Err(KubericError::StaleAgentControlVersion { .. })
        ));

        let request =
            versioned_request(&agent, "versioned", DurableReplicaAction::RevokeWriteStatus);
        let (reply_tx, _reply_rx) = oneshot::channel();
        agent.accept_versioned_correlated(request, reply_tx);
        assert_eq!(agent.control_version.value(), 1);
        assert!(runtime_rx.recv().await.is_some());
    }

    #[tokio::test]
    async fn unretained_old_version_fails_as_continuity_unavailable() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), false);
        let (direct_tx, _direct_rx) = oneshot::channel();
        agent.accept_direct(PendingDirect {
            effect: RuntimeEffect::Open {
                mode: OpenMode::New,
            },
            reply: ClientReply::Unit(direct_tx),
            close_barrier: false,
        });
        assert!(runtime_rx.recv().await.is_some());

        let mut request =
            versioned_request(&agent, "unknown", DurableReplicaAction::RevokeWriteStatus);
        request.expected_control_version = AgentControlVersion::new(0);
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_versioned_correlated(request, reply_tx);
        assert!(matches!(
            reply_rx.await.unwrap(),
            Err(KubericError::CorrelatedContinuityUnavailable { .. })
        ));
        assert_eq!(agent.control_version.value(), 1);
    }

    #[tokio::test]
    async fn same_pod_restart_rejects_prior_generation_after_ambiguous_completion() {
        let (mut old_agent, mut old_runtime_rx) = test_agent(Epoch::default(), false);
        let request = versioned_request(
            &old_agent,
            "ambiguous",
            DurableReplicaAction::RevokeWriteStatus,
        );
        let retry = request.clone();
        let (reply_tx, reply_rx) = oneshot::channel();
        old_agent.accept_versioned_correlated(request, reply_tx);
        drop(reply_rx);
        let runtime = old_runtime_rx.recv().await.unwrap();
        assert!(runtime.reply.send(Ok(RuntimeEffectResult::Unit)).is_ok());
        let completion = old_agent.completion_rx.recv().await.unwrap();
        old_agent.handle_completion(completion);
        assert_eq!(old_agent.terminals.len(), 1);

        let (mut new_agent, mut new_runtime_rx) = test_agent(Epoch::default(), false);
        assert_eq!(new_agent.instance_id, old_agent.instance_id);
        assert_ne!(new_agent.generation, old_agent.generation);
        let (retry_tx, retry_rx) = oneshot::channel();
        new_agent.accept_versioned_correlated(retry, retry_tx);
        assert!(matches!(
            retry_rx.await.unwrap(),
            Err(KubericError::StaleAgentGeneration { .. })
        ));
        assert!(new_runtime_rx.try_recv().is_err());
        assert!(new_agent.terminals.is_empty());
    }

    #[tokio::test]
    async fn shutdown_drain_keeps_status_readable_and_rejects_mutations() {
        let shutdown = CancellationToken::new();
        let (command_tx, command_rx) = mpsc::channel(16);
        let (runtime_tx, mut runtime_rx) = mpsc::channel(16);
        let (_fault_tx, fault_rx) = mpsc::channel(16);
        let (_status_tx, status_rx) = watch::channel(RuntimeControlSnapshot {
            instance_id: ReplicaInstanceId::new("pod-uid"),
            role: Role::Primary,
            epoch: Epoch::new(1, 1),
            configuration: None,
            election_configuration: None,
            deactivation_info: None,
            partition_state: Some(Arc::new(PartitionState::new())),
        });
        let agent = ReplicaAgent::new(
            1,
            ReplicaInstanceId::new("pod-uid"),
            command_rx,
            runtime_tx,
            status_rx,
            fault_rx,
            shutdown.clone(),
        );
        let task = tokio::spawn(agent.serve());

        let (action_tx, _action_rx) = oneshot::channel();
        command_tx
            .send(AgentCommand::ExecuteDurableAction {
                action_id: "pending".to_string(),
                action: DurableReplicaAction::RevokeWriteStatus,
                reply: action_tx,
            })
            .await
            .unwrap();
        let runtime = runtime_rx.recv().await.unwrap();
        shutdown.cancel();

        let (status_tx, status_rx) = oneshot::channel();
        command_tx
            .send(AgentCommand::GetStatus { reply: status_tx })
            .await
            .unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(1), status_rx)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            status.durable_action.unwrap().state,
            DurableActionState::InProgress
        );

        let (mutation_tx, mutation_rx) = oneshot::channel();
        command_tx
            .send(AgentCommand::UpdateEpoch {
                epoch: Epoch::new(1, 2),
                reply: mutation_tx,
            })
            .await
            .unwrap();
        assert!(matches!(
            mutation_rx.await.unwrap(),
            Err(KubericError::Closed)
        ));

        assert!(runtime.reply.send(Ok(RuntimeEffectResult::Unit)).is_ok());
        task.await.unwrap();
    }

    #[tokio::test]
    async fn direct_mutations_run_fifo_and_close_is_a_queue_barrier() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), false);
        let (open_tx, open_rx) = oneshot::channel();
        agent.accept_direct(PendingDirect {
            effect: RuntimeEffect::Open {
                mode: OpenMode::New,
            },
            reply: ClientReply::Unit(open_tx),
            close_barrier: false,
        });
        let (epoch_tx, epoch_rx) = oneshot::channel();
        agent.accept_direct(PendingDirect {
            effect: RuntimeEffect::UpdateEpoch {
                epoch: Epoch::new(1, 1),
            },
            reply: ClientReply::Unit(epoch_tx),
            close_barrier: false,
        });
        let (close_tx, close_rx) = oneshot::channel();
        agent.accept_direct(PendingDirect {
            effect: RuntimeEffect::Close {
                terminate_runtime: true,
            },
            reply: ClientReply::Unit(close_tx),
            close_barrier: true,
        });
        let version_at_close = agent.control_version;

        let (late_tx, late_rx) = oneshot::channel();
        agent.accept_direct(PendingDirect {
            effect: RuntimeEffect::UpdateEpoch {
                epoch: Epoch::new(1, 2),
            },
            reply: ClientReply::Unit(late_tx),
            close_barrier: false,
        });
        assert!(matches!(late_rx.await.unwrap(), Err(KubericError::Closed)));
        assert_eq!(agent.control_version, version_at_close);

        let open = runtime_rx.recv().await.unwrap();
        assert!(matches!(open.effect, RuntimeEffect::Open { .. }));
        assert!(open.reply.send(Ok(RuntimeEffectResult::Unit)).is_ok());
        let completion = agent.completion_rx.recv().await.unwrap();
        agent.handle_completion(completion);
        open_rx.await.unwrap().unwrap();

        let update = runtime_rx.recv().await.unwrap();
        assert!(matches!(
            update.effect,
            RuntimeEffect::UpdateEpoch {
                epoch: Epoch {
                    data_loss_number: 1,
                    configuration_number: 1
                }
            }
        ));
        assert!(update.reply.send(Ok(RuntimeEffectResult::Unit)).is_ok());
        let completion = agent.completion_rx.recv().await.unwrap();
        agent.handle_completion(completion);
        epoch_rx.await.unwrap().unwrap();

        let close = runtime_rx.recv().await.unwrap();
        assert!(matches!(
            close.effect,
            RuntimeEffect::Close {
                terminate_runtime: true
            }
        ));
        assert!(close.reply.send(Ok(RuntimeEffectResult::Unit)).is_ok());
        let completion = agent.completion_rx.recv().await.unwrap();
        agent.handle_completion(completion);
        close_rx.await.unwrap().unwrap();
        assert!(agent.shutting_down);
        assert!(agent.direct_queue.is_empty());
    }

    #[tokio::test]
    async fn mismatched_and_late_runtime_completions_cannot_replace_active_state() {
        let (mut agent, mut runtime_rx) = test_agent(Epoch::default(), false);
        let (reply_tx, reply_rx) = oneshot::channel();
        agent.accept_direct(PendingDirect {
            effect: RuntimeEffect::Open {
                mode: OpenMode::New,
            },
            reply: ClientReply::Unit(reply_tx),
            close_barrier: false,
        });
        let runtime = runtime_rx.recv().await.unwrap();
        let active_id = match agent.active.as_ref().unwrap() {
            ActiveMutation::Direct(active) => active.execution_id.clone(),
            ActiveMutation::Correlated(_) => panic!("expected direct mutation"),
        };
        agent.handle_completion(RuntimeCompletion {
            execution_id: RuntimeEffectExecutionId {
                generation: active_id.generation.clone(),
                sequence: active_id.sequence + 1,
            },
            result: Ok(RuntimeEffectResult::Unit),
        });
        assert!(agent.active.is_some());

        assert!(runtime.reply.send(Ok(RuntimeEffectResult::Unit)).is_ok());
        let completion = agent.completion_rx.recv().await.unwrap();
        agent.handle_completion(completion);
        reply_rx.await.unwrap().unwrap();
        assert!(agent.active.is_none());

        agent.handle_completion(RuntimeCompletion {
            execution_id: active_id,
            result: Ok(RuntimeEffectResult::Unit),
        });
        assert!(agent.active.is_none());
    }

    #[test]
    fn ascii_error_is_truncated_to_exact_bound() {
        let normalized = normalize_error(&"x".repeat(MAX_ERROR_BYTES + 20));
        assert_eq!(normalized.len(), MAX_ERROR_BYTES);
    }

    #[test]
    fn tracing_uses_bounded_payloads_and_expected_transition_levels() {
        let writer = CapturedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(writer.clone())
            .finish();

        tracing::subscriber::with_default(subscriber, || {
            let (mut agent, _runtime_rx) = test_agent(Epoch::default(), false);
            agent.record_fault(FaultType::Transient);
            let retained = CorrelatedActionObservation {
                generation: agent.generation.clone(),
                control_version: AgentControlVersion::new(1),
                admission: CorrelatedActionAdmission::Legacy,
                action: DurableActionObservation {
                    action_id: "retained".to_string(),
                    signature: DurableReplicaAction::Close.signature(),
                    state: DurableActionState::Completed,
                    error: None,
                    result: None,
                },
            };
            agent.retain_terminal(retained);
            let (replay_tx, _replay_rx) = oneshot::channel();
            agent.accept_legacy_correlated(
                "retained".to_string(),
                DurableReplicaAction::Close,
                replay_tx,
            );
            let (conflict_tx, _conflict_rx) = oneshot::channel();
            agent.accept_legacy_correlated(
                "retained".to_string(),
                DurableReplicaAction::RevokeWriteStatus,
                conflict_tx,
            );

            let observation = CorrelatedActionObservation {
                generation: agent.generation.clone(),
                control_version: AgentControlVersion::new(2),
                admission: CorrelatedActionAdmission::Legacy,
                action: DurableActionObservation {
                    action_id: "failed".to_string(),
                    signature: "failed".to_string(),
                    state: DurableActionState::InProgress,
                    error: None,
                    result: None,
                },
            };
            let execution_id = agent.next_execution_id();
            agent.active = Some(ActiveMutation::Correlated(ActiveCorrelated {
                execution_id: execution_id.clone(),
                observation,
                reply: None,
            }));
            agent.handle_completion(RuntimeCompletion {
                execution_id,
                result: Err(KubericError::Internal("x".repeat(2048).into())),
            });
        });

        let output = writer.contents();
        assert!(output.contains("DEBUG"));
        assert!(output.contains("WARN"));
        assert!(output.contains("replaying correlated action observation"));
        assert!(output.contains("correlated action failed"));
        assert!(!output.contains(&"x".repeat(MAX_ERROR_BYTES + 1)));
    }
}
