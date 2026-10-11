use kuberic_runtime::protocol::command::{ProtocolCommand, PublicV1SessionRenewalCommand};
use kuberic_runtime::protocol::public_operations::{
    ProcessSessionRenewalRequired, PublicSessionEpochBarrierStage, PublicSessionRenewalStage,
    PublicV1AgentReport, PublicV1SessionRenewalStatus,
};
use kuberic_runtime::protocol::types::{
    Epoch, OperationId, ProcessSessionEndpoint, ProcessSessionId, ReplicaRole,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicV1SessionRenewalPlan {
    PersistStatus {
        expected: Option<Box<PublicV1SessionRenewalStatus>>,
        next: Box<PublicV1SessionRenewalStatus>,
    },
    Dispatch {
        expected: Box<PublicV1SessionRenewalStatus>,
        command: ProtocolCommand,
    },
    WaitForSurvivor {
        expected: Box<PublicV1SessionRenewalStatus>,
        survivor: Box<ProcessSessionEndpoint>,
    },
    Stable,
}

pub fn accept_session_renewal(
    accepted: Option<PublicV1SessionRenewalStatus>,
    required: ProcessSessionRenewalRequired,
    replacement_report: Option<&PublicV1AgentReport>,
) -> Result<PublicV1SessionRenewalPlan, String> {
    required.validate().map_err(str::to_owned)?;
    if required.replacement.is_some() {
        required
            .validate_replacement_report(
                replacement_report
                    .ok_or_else(|| "session renewal lacks replacement report".to_string())?,
            )
            .map_err(str::to_owned)?;
    } else if replacement_report.is_some() {
        return Err("peer withdrawal cannot carry a replacement report".into());
    }
    if let Some(current) = &accepted {
        current.validate().map_err(str::to_owned)?;
        if current.required == required {
            return Ok(PublicV1SessionRenewalPlan::Stable);
        }
        return Err("accepted session renewal changed before completion".into());
    }
    Ok(PublicV1SessionRenewalPlan::PersistStatus {
        expected: None,
        next: Box::new(PublicV1SessionRenewalStatus::required(required).map_err(str::to_owned)?),
    })
}

pub fn plan_session_renewal(
    accepted: &PublicV1SessionRenewalStatus,
    unreachable_sessions: &[ProcessSessionId],
) -> Result<PublicV1SessionRenewalPlan, String> {
    accepted.validate().map_err(str::to_owned)?;
    match accepted.stage {
        PublicSessionRenewalStage::Required => {
            let command = PublicV1SessionRenewalCommand::CloseAccessAndIngress {
                accepted: accepted.clone(),
            };
            command.validate().map_err(str::to_owned)?;
            Ok(PublicV1SessionRenewalPlan::Dispatch {
                expected: Box::new(accepted.clone()),
                command: ProtocolCommand::PublicV1History(Box::new(
                    kuberic_runtime::protocol::command::PublicV1HistoryCommand::SessionRenewal(
                        Box::new(command),
                    ),
                )),
            })
        }
        PublicSessionRenewalStage::AccessClosed => Ok(PublicV1SessionRenewalPlan::PersistStatus {
            expected: Some(Box::new(accepted.clone())),
            next: Box::new(accepted.freeze_surviving_set().map_err(str::to_owned)?),
        }),
        PublicSessionRenewalStage::SurvivingSetFrozen => {
            Ok(PublicV1SessionRenewalPlan::PersistStatus {
                expected: Some(Box::new(accepted.clone())),
                next: Box::new(accepted.begin_epoch_barriers().map_err(str::to_owned)?),
            })
        }
        PublicSessionRenewalStage::EpochBarriersPending => {
            if let Some(barrier) = accepted
                .barriers
                .iter()
                .find(|barrier| barrier.stage == PublicSessionEpochBarrierStage::Pending)
            {
                if unreachable_sessions.contains(&barrier.survivor.process_session_id) {
                    return Ok(PublicV1SessionRenewalPlan::WaitForSurvivor {
                        expected: Box::new(accepted.clone()),
                        survivor: Box::new(barrier.survivor.clone()),
                    });
                }
                let command = PublicV1SessionRenewalCommand::UpdateEpochBarrier {
                    accepted: accepted.clone(),
                    survivor: barrier.survivor.clone(),
                    epoch: accepted.required.renewal_epoch,
                };
                command.validate().map_err(str::to_owned)?;
                return Ok(PublicV1SessionRenewalPlan::Dispatch {
                    expected: Box::new(accepted.clone()),
                    command: ProtocolCommand::PublicV1History(Box::new(
                        kuberic_runtime::protocol::command::PublicV1HistoryCommand::SessionRenewal(
                            Box::new(command),
                        ),
                    )),
                });
            }
            Ok(PublicV1SessionRenewalPlan::PersistStatus {
                expected: Some(Box::new(accepted.clone())),
                next: Box::new(accepted.finish_epoch_barriers().map_err(str::to_owned)?),
            })
        }
        PublicSessionRenewalStage::ReplacementRolePending => {
            let replacement = accepted
                .required
                .replacement
                .as_ref()
                .expect("validated replacement-role stage");
            let command = PublicV1SessionRenewalCommand::ReplayReplacementRole {
                accepted: accepted.clone(),
                process_session_id: replacement.process_session_id.clone(),
                epoch: accepted.required.renewal_epoch,
                role: accepted.required.predecessor.role,
            };
            command.validate().map_err(str::to_owned)?;
            Ok(PublicV1SessionRenewalPlan::Dispatch {
                expected: Box::new(accepted.clone()),
                command: ProtocolCommand::PublicV1History(Box::new(
                    kuberic_runtime::protocol::command::PublicV1HistoryCommand::SessionRenewal(
                        Box::new(command),
                    ),
                )),
            })
        }
        PublicSessionRenewalStage::ConfigurationPending => {
            let command = PublicV1SessionRenewalCommand::InstallFreshConfiguration {
                accepted: accepted.clone(),
                configuration: accepted.required.fresh_configuration.clone(),
            };
            command.validate().map_err(str::to_owned)?;
            Ok(PublicV1SessionRenewalPlan::Dispatch {
                expected: Box::new(accepted.clone()),
                command: ProtocolCommand::PublicV1History(Box::new(
                    kuberic_runtime::protocol::command::PublicV1HistoryCommand::SessionRenewal(
                        Box::new(command),
                    ),
                )),
            })
        }
        PublicSessionRenewalStage::Authorized | PublicSessionRenewalStage::Retired => {
            Ok(PublicV1SessionRenewalPlan::Stable)
        }
    }
}

pub fn accept_access_closed(
    accepted: &PublicV1SessionRenewalStatus,
) -> Result<PublicV1SessionRenewalStatus, String> {
    accepted.close_access().map_err(str::to_owned)
}

pub fn accept_epoch_updated(
    accepted: &PublicV1SessionRenewalStatus,
    survivor: &ProcessSessionEndpoint,
    epoch: Epoch,
) -> Result<PublicV1SessionRenewalStatus, String> {
    accepted
        .record_epoch_updated(survivor, epoch)
        .map_err(str::to_owned)
}

pub fn accept_survivor_retired(
    accepted: &PublicV1SessionRenewalStatus,
    survivor: &ProcessSessionEndpoint,
    retirement_operation_id: OperationId,
) -> Result<PublicV1SessionRenewalStatus, String> {
    accepted
        .exclude_survivor(survivor, retirement_operation_id)
        .map_err(str::to_owned)
}

pub fn accept_replacement_role(
    accepted: &PublicV1SessionRenewalStatus,
    process_session_id: &ProcessSessionId,
    epoch: Epoch,
    role: ReplicaRole,
) -> Result<PublicV1SessionRenewalStatus, String> {
    accepted
        .replay_replacement_role(process_session_id, epoch, role)
        .map_err(str::to_owned)
}

pub fn accept_fresh_configuration(
    accepted: &PublicV1SessionRenewalStatus,
) -> Result<PublicV1SessionRenewalStatus, String> {
    accepted
        .install_fresh_configuration(&accepted.required.fresh_configuration)
        .map_err(str::to_owned)
}
