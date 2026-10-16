use kuberic_runtime::protocol::command::PublicV1OperationCommand;
use kuberic_runtime::protocol::public_operations::{
    PublicBuildInput, PublicCatchUpMode, PublicConfiguration, PublicOperationClass,
    PublicOperationIntent, PublicOperationProgram, PublicReplicaDescriptor, PublicV1AcceptedResult,
    PublicV1AcceptedStatus, PublicV1AgentReport, PublicV1CallKind, PublicV1OperationResult,
};
use kuberic_runtime::protocol::types::{
    ConfigurationDescriptor, ConfigurationId, ConfigurationMember, Epoch, OperationId, ReplicaId,
    ReplicaRole,
};

use super::public_v1_evidence::{
    PublicV1AcceptedTransitionIntent, PublicV1AcceptedTransitionStatus, PublicV1EvidenceConsumer,
    PublicV1ResourceAuthority,
};

#[derive(Debug, Clone)]
pub struct PublicV1TopologyReplica {
    pub report: PublicV1AgentReport,
    pub replication_address: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicV1TopologyPlan {
    Begin {
        transition: Box<PublicV1AcceptedTransitionStatus>,
        command: Box<PublicV1OperationCommand>,
    },
    PersistTopology {
        expected: Option<ConfigurationId>,
        next: Box<ConfigurationDescriptor>,
    },
    RoutePrimary {
        primary: kuberic_runtime::protocol::types::ReplicaIdentity,
    },
    Wait,
    Stable,
}

pub fn plan_public_v1_topology(
    accepted: &PublicV1AcceptedStatus,
    current: Option<&ConfigurationDescriptor>,
    replicas: &[PublicV1TopologyReplica],
    desired_replicas: u32,
    switchover_target: Option<ReplicaId>,
    routed_primary: Option<ReplicaId>,
    resource_version: &str,
) -> Result<PublicV1TopologyPlan, String> {
    accepted.validate_evidence().map_err(str::to_owned)?;
    if desired_replicas == 0 || resource_version.is_empty() {
        return Err("public-v1 topology input is incomplete".into());
    }
    let mut observed = replicas
        .iter()
        .filter(|replica| {
            replica.report.identity == accepted.identity
                && replica.report.resource_uid == accepted.history.resource_uid
                && replica.report.history == accepted.history
                && replica.report.replica.replica_id.value() > 0
        })
        .cloned()
        .collect::<Vec<_>>();
    observed.sort_by_key(|replica| replica.report.replica.replica_id);
    observed.dedup_by_key(|replica| replica.report.replica.replica_id);
    let eligible = observed
        .iter()
        .filter(|replica| replica.report.replica.replica_id.value() <= i64::from(desired_replicas))
        .cloned()
        .collect::<Vec<_>>();
    if current.is_none() && eligible.len() != desired_replicas as usize {
        return Ok(PublicV1TopologyPlan::Wait);
    }
    if let Some(current) = current
        && eligible.len() < majority(current.members.len()) as usize
    {
        return Ok(PublicV1TopologyPlan::Wait);
    }

    let primary_id = choose_primary(current, &eligible, switchover_target)?;
    let next = desired_configuration(current, &eligible, primary_id)?;
    if current == Some(&next) && configuration_is_admitted(&next, &eligible) {
        return Ok(PublicV1TopologyPlan::Stable);
    }
    let transfer = current
        .filter(|configuration| {
            configuration.primary_id != next.primary_id
                && !next
                    .members
                    .iter()
                    .any(|member| member.identity.replica_id == configuration.primary_id)
                && observed
                    .iter()
                    .any(|replica| replica.report.replica.replica_id == configuration.primary_id)
        })
        .map(|configuration| {
            let members = configuration
                .members
                .iter()
                .cloned()
                .map(|mut member| {
                    member.role = if member.identity.replica_id == next.primary_id {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    };
                    member
                })
                .collect();
            ConfigurationDescriptor::new(
                Epoch::new(
                    configuration.epoch.data_loss_number,
                    configuration.epoch.configuration_number + 1,
                ),
                next.primary_id,
                members,
                configuration.write_quorum,
            )
        });
    let operational_next = transfer.as_ref().unwrap_or(&next);
    let operational_replicas = if transfer.is_some() {
        &observed
    } else {
        &eligible
    };
    if let Some(replica) = operational_replicas
        .iter()
        .find(|replica| replica.replication_address.is_empty())
    {
        let open = PublicOperationProgram::Open {
            replica: replica.report.replica.clone(),
            existing: true,
        };
        let open_configuration_id = if current.is_some_and(|configuration| {
            !configuration
                .members
                .iter()
                .any(|member| member.identity == replica.report.replica)
        }) {
            &next.configuration_id
        } else {
            &operational_next.configuration_id
        };
        let open_id = content_addressed_operation_id(
            format!(
                "topology-{}-{}-{}-open",
                open_configuration_id,
                replica.report.replica.replica_id,
                replica.report.process_session_id,
            ),
            &open,
        );
        if accepted.completed_operations.iter().any(|completion| {
            completion.operation_id == open_id
                && completion.result == PublicV1AcceptedResult::Succeeded
        }) {
            return Ok(PublicV1TopologyPlan::Wait);
        }
        return begin_operation(accepted, replica, open, open_id, resource_version).map(
            |(transition, command)| PublicV1TopologyPlan::Begin {
                transition: Box::new(transition),
                command: Box::new(command),
            },
        );
    }
    let ordinary_failover = current.is_some_and(|configuration| {
        configuration.primary_id != operational_next.primary_id
            && !observed.iter().any(|replica| {
                configuration.members.iter().any(|member| {
                    member.role == ReplicaRole::Primary && member.identity == replica.report.replica
                })
            })
    });
    if let Some(current) = current
        && current.primary_id != operational_next.primary_id
        && let Some(source) = observed.iter().find(|replica| {
            current.members.iter().any(|member| {
                member.role == ReplicaRole::Primary && member.identity == replica.report.replica
            })
        })
    {
        let catch_up = PublicOperationProgram::CatchUp {
            configuration: public_configuration(&source.report.replica, &observed, current)?,
            mode: PublicCatchUpMode::All,
        };
        let catch_up_id = content_addressed_operation_id(
            format!(
                "topology-{}-catch-up-{}",
                current.configuration_id, operational_next.primary_id
            ),
            &catch_up,
        );
        if !accepted.completed_operations.iter().any(|completion| {
            completion.operation_id == catch_up_id
                && completion.result == PublicV1AcceptedResult::Succeeded
        }) {
            return begin_operation(accepted, source, catch_up, catch_up_id, resource_version).map(
                |(transition, command)| PublicV1TopologyPlan::Begin {
                    transition: Box::new(transition),
                    command: Box::new(command),
                },
            );
        }
        let swap = planned_swap_program(current, operational_next, &observed, source)?;
        let swap_id = content_addressed_operation_id(
            format!(
                "topology-{}-swap-{}",
                current.configuration_id, operational_next.primary_id
            ),
            &swap,
        );
        if !accepted.completed_operations.iter().any(|completion| {
            completion.operation_id == swap_id
                && completion.result == PublicV1AcceptedResult::Succeeded
        }) {
            return begin_swap(
                accepted,
                operational_next,
                source,
                swap,
                swap_id,
                resource_version,
            )
            .map(|(transition, command)| PublicV1TopologyPlan::Begin {
                transition: Box::new(transition),
                command: Box::new(command),
            });
        }
    }
    if let Some(current) = current
        && let Some(target) = eligible.iter().find(|replica| {
            !current
                .members
                .iter()
                .any(|member| member.identity == replica.report.replica)
        })
    {
        let open = PublicOperationProgram::Open {
            replica: target.report.replica.clone(),
            existing: true,
        };
        let open_id = content_addressed_operation_id(
            format!(
                "topology-{}-{}-{}-open",
                next.configuration_id,
                target.report.replica.replica_id,
                target.report.process_session_id,
            ),
            &open,
        );
        if !accepted.completed_operations.iter().any(|completion| {
            completion.operation_id == open_id
                && completion.result == PublicV1AcceptedResult::Succeeded
        }) {
            return begin_operation(accepted, target, open, open_id, resource_version).map(
                |(transition, command)| PublicV1TopologyPlan::Begin {
                    transition: Box::new(transition),
                    command: Box::new(command),
                },
            );
        }
        for (step, program) in [
            (
                "build-role",
                PublicOperationProgram::Role {
                    epoch: current.epoch,
                    role: ReplicaRole::IdleSecondary,
                },
            ),
            (
                "build-epoch",
                PublicOperationProgram::Epoch {
                    epoch: current.epoch,
                },
            ),
        ] {
            let operation_id = content_addressed_operation_id(
                format!(
                    "topology-{}-{}-{}-{step}",
                    current.configuration_id,
                    target.report.replica.replica_id,
                    target.report.process_session_id,
                ),
                &program,
            );
            if accepted.completed_operations.iter().any(|completion| {
                completion.operation_id == operation_id
                    && completion.result == PublicV1AcceptedResult::Succeeded
            }) {
                continue;
            }
            return begin_operation(accepted, target, program, operation_id, resource_version).map(
                |(transition, command)| PublicV1TopologyPlan::Begin {
                    transition: Box::new(transition),
                    command: Box::new(command),
                },
            );
        }
        let source = eligible
            .iter()
            .find(|replica| {
                current.members.iter().any(|member| {
                    member.role == ReplicaRole::Primary && member.identity == replica.report.replica
                })
            })
            .or_else(|| {
                eligible
                    .iter()
                    .find(|replica| replica.report.replica.replica_id == primary_id)
            })
            .ok_or_else(|| "public-v1 build lacks an available source".to_string())?;
        let build_id = OperationId::new(format!(
            "topology-{}-build-{}-{}-{}-{}",
            current.configuration_id,
            target.report.replica.replica_id,
            source.report.process_session_id,
            target.report.process_session_id,
            target.report.storage_incarnation.storage_generation,
        ));
        if !accepted.completed_operations.iter().any(|completion| {
            completion.operation_id == build_id
                && completion.result == PublicV1AcceptedResult::Succeeded
        }) {
            let source_is_accepted_primary = current.members.iter().any(|member| {
                member.role == ReplicaRole::Primary && member.identity == source.report.replica
            });
            if !source_is_accepted_primary {
                let survivors = eligible
                    .iter()
                    .filter(|replica| replica.report.replica != target.report.replica)
                    .cloned()
                    .collect::<Vec<_>>();
                return plan_public_v1_topology(
                    accepted,
                    Some(current),
                    &survivors,
                    desired_replicas,
                    None,
                    routed_primary,
                    resource_version,
                );
            }
            if source.report.role != ReplicaRole::Primary
                || source.report.write_status
                    != kuberic_runtime::protocol::types::AccessStatus::Granted
            {
                return Ok(PublicV1TopologyPlan::Wait);
            }
            return begin_build(
                accepted,
                current,
                source,
                target,
                build_id,
                resource_version,
            )
            .map(|(transition, command)| PublicV1TopologyPlan::Begin {
                transition: Box::new(transition),
                command: Box::new(command),
            });
        }
    }
    let mut ordered_replicas = operational_replicas.iter().collect::<Vec<_>>();
    if ordinary_failover {
        ordered_replicas.sort_by_key(|replica| {
            replica.report.replica.replica_id == operational_next.primary_id
        });
    }
    for replica in ordered_replicas {
        let prebuilt = current.is_some_and(|configuration| {
            !configuration
                .members
                .iter()
                .any(|member| member.identity == replica.report.replica)
        });
        for (step, program) in operation_sequence(
            replica,
            operational_replicas,
            operational_next,
            prebuilt,
            !ordinary_failover,
            ordinary_failover && replica.report.replica.replica_id == operational_next.primary_id,
        )? {
            let operation_id = content_addressed_operation_id(
                format!(
                    "topology-{}-{}-{}-{}",
                    operational_next.configuration_id,
                    replica.report.replica.replica_id,
                    replica.report.process_session_id,
                    step
                ),
                &program,
            );
            if accepted.completed_operations.iter().any(|completion| {
                completion.operation_id == operation_id
                    && completion.result == PublicV1AcceptedResult::Succeeded
            }) {
                continue;
            }

            return begin_operation(accepted, replica, program, operation_id, resource_version)
                .map(|(transition, command)| PublicV1TopologyPlan::Begin {
                    transition: Box::new(transition),
                    command: Box::new(command),
                });
        }
    }

    if let Some(current) = current
        && current.primary_id != operational_next.primary_id
    {
        let primary = operational_replicas
            .iter()
            .find(|replica| replica.report.replica.replica_id == operational_next.primary_id)
            .ok_or_else(|| "public-v1 transfer target is unavailable".to_string())?;
        if primary.report.role != ReplicaRole::Primary
            || primary.report.write_status
                != kuberic_runtime::protocol::types::AccessStatus::Granted
            || observed.iter().any(|replica| {
                replica.report.replica.replica_id != next.primary_id
                    && replica.report.write_status
                        == kuberic_runtime::protocol::types::AccessStatus::Granted
            })
        {
            return Ok(PublicV1TopologyPlan::Wait);
        }
        if routed_primary != Some(operational_next.primary_id) {
            return Ok(PublicV1TopologyPlan::RoutePrimary {
                primary: primary.report.replica.clone(),
            });
        }
    }

    if let Some(transfer) = transfer
        && current != Some(&transfer)
    {
        return Ok(PublicV1TopologyPlan::PersistTopology {
            expected: current.map(|configuration| configuration.configuration_id.clone()),
            next: Box::new(transfer),
        });
    }

    if let Some(current) = current
        && let Some(removed_member) = current.members.iter().find(|member| {
            !eligible
                .iter()
                .any(|replica| replica.report.replica == member.identity)
        })
        && let Some(target) = observed
            .iter()
            .find(|replica| replica.report.replica == removed_member.identity)
    {
        let source = eligible
            .iter()
            .find(|replica| replica.report.replica.replica_id == next.primary_id)
            .ok_or_else(|| "public-v1 removal lacks retained primary".to_string())?;
        let remove_id = OperationId::new(format!(
            "topology-{}-remove-{}-{}-{}-{}",
            current.configuration_id,
            target.report.replica.replica_id,
            source.report.process_session_id,
            target.report.process_session_id,
            target.report.storage_incarnation.storage_generation,
        ));
        if !accepted.completed_operations.iter().any(|completion| {
            completion.operation_id == remove_id
                && completion.result == PublicV1AcceptedResult::Succeeded
        }) {
            return begin_remove(
                accepted,
                current,
                &next,
                source,
                target,
                remove_id,
                resource_version,
            )
            .map(|(transition, command)| PublicV1TopologyPlan::Begin {
                transition: Box::new(transition),
                command: Box::new(command),
            });
        }
    }

    if current != Some(&next) {
        return Ok(PublicV1TopologyPlan::PersistTopology {
            expected: current.map(|configuration| configuration.configuration_id.clone()),
            next: Box::new(next),
        });
    }

    fn begin_remove(
        accepted: &PublicV1AcceptedStatus,
        current: &ConfigurationDescriptor,
        next: &ConfigurationDescriptor,
        source: &PublicV1TopologyReplica,
        target: &PublicV1TopologyReplica,
        operation_id: OperationId,
        resource_version: &str,
    ) -> Result<(PublicV1AcceptedTransitionStatus, PublicV1OperationCommand), String> {
        let remove = PublicBuildInput {
            attempt: operation_id.clone(),
            epoch: next.epoch,
            current_configuration_id: Some(current.configuration_id.clone()),
            replica: target.report.replica.clone(),
            target_storage: target.report.storage_incarnation.clone(),
            process_session_id: target.report.process_session_id.clone(),
            replication_address: target.replication_address.clone(),
            history: Some(accepted.history.clone()),
        };
        let program = PublicOperationProgram::Remove(remove);
        let revision = retained_authority_revision(accepted, source, "removal")?;
        let intent = PublicOperationIntent {
            identity: accepted.identity.clone(),
            operation_id: operation_id.clone(),
            revision,
            process_session_id: source.report.process_session_id.clone(),
            class: PublicOperationClass::Remove {
                target: target.report.replica.replica_id,
            },
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        };
        intent.validate().map_err(str::to_owned)?;
        let transition = PublicV1AcceptedTransitionStatus {
            resource_version: resource_version.to_string(),
            evidence: accepted.clone(),
            intent: PublicV1AcceptedTransitionIntent {
                consumer: PublicV1EvidenceConsumer::ScaleDown,
                operation_id,
                revision,
                call_kind: PublicV1CallKind::Remove,
                history: accepted.history.clone(),
                source: PublicV1ResourceAuthority {
                    storage_incarnation: source.report.storage_incarnation.clone(),
                    resource_version: resource_version.to_string(),
                },
                target: Some(PublicV1ResourceAuthority {
                    storage_incarnation: target.report.storage_incarnation.clone(),
                    resource_version: resource_version.to_string(),
                }),
                process_session_id: source.report.process_session_id.clone(),
                input_digest: intent.input_digest.clone(),
                replay_disposition: intent.replay_disposition,
                previous_configuration_id: None,
                current_configuration_id: None,
                catch_up_mode: None,
                catch_up_specific_quorum: None,
            },
        };
        transition.validate()?;
        Ok((
            transition,
            PublicV1OperationCommand {
                target: source.report.replica.clone(),
                intent,
            },
        ))
    }
    let primary = eligible
        .iter()
        .find(|replica| replica.report.replica.replica_id == primary_id)
        .expect("selected primary is eligible");
    if primary.report.role != ReplicaRole::Primary
        || primary.report.write_status != kuberic_runtime::protocol::types::AccessStatus::Granted
    {
        return Ok(PublicV1TopologyPlan::Wait);
    }
    Ok(PublicV1TopologyPlan::Stable)
}

fn begin_build(
    accepted: &PublicV1AcceptedStatus,
    current: &ConfigurationDescriptor,
    source: &PublicV1TopologyReplica,
    target: &PublicV1TopologyReplica,
    operation_id: OperationId,
    resource_version: &str,
) -> Result<(PublicV1AcceptedTransitionStatus, PublicV1OperationCommand), String> {
    let build = PublicBuildInput {
        attempt: operation_id.clone(),
        epoch: current.epoch,
        current_configuration_id: Some(current.configuration_id.clone()),
        replica: target.report.replica.clone(),
        target_storage: target.report.storage_incarnation.clone(),
        process_session_id: target.report.process_session_id.clone(),
        replication_address: target.replication_address.clone(),
        history: Some(accepted.history.clone()),
    };
    let program = PublicOperationProgram::Build(build);
    let revision = retained_authority_revision(accepted, source, "build")?;
    let intent = PublicOperationIntent {
        identity: accepted.identity.clone(),
        operation_id: operation_id.clone(),
        revision,
        process_session_id: source.report.process_session_id.clone(),
        class: PublicOperationClass::Build {
            target: target.report.replica.replica_id,
        },
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program),
    };
    intent.validate().map_err(str::to_owned)?;
    let consumer = if current
        .members
        .iter()
        .any(|member| member.identity.replica_id == target.report.replica.replica_id)
    {
        PublicV1EvidenceConsumer::RepairReplacement
    } else {
        PublicV1EvidenceConsumer::ScaleUp
    };
    let transition = PublicV1AcceptedTransitionStatus {
        resource_version: resource_version.to_string(),
        evidence: accepted.clone(),
        intent: PublicV1AcceptedTransitionIntent {
            consumer,
            operation_id,
            revision,
            call_kind: PublicV1CallKind::Build,
            history: accepted.history.clone(),
            source: PublicV1ResourceAuthority {
                storage_incarnation: source.report.storage_incarnation.clone(),
                resource_version: resource_version.to_string(),
            },
            target: Some(PublicV1ResourceAuthority {
                storage_incarnation: target.report.storage_incarnation.clone(),
                resource_version: resource_version.to_string(),
            }),
            process_session_id: source.report.process_session_id.clone(),
            input_digest: intent.input_digest.clone(),
            replay_disposition: intent.replay_disposition,
            previous_configuration_id: None,
            current_configuration_id: Some(current.configuration_id.clone()),
            catch_up_mode: None,
            catch_up_specific_quorum: None,
        },
    };
    transition.validate()?;
    Ok((
        transition,
        PublicV1OperationCommand {
            target: source.report.replica.clone(),
            intent,
        },
    ))
}

fn begin_swap(
    accepted: &PublicV1AcceptedStatus,
    next: &ConfigurationDescriptor,
    source: &PublicV1TopologyReplica,
    program: PublicOperationProgram,
    operation_id: OperationId,
    resource_version: &str,
) -> Result<(PublicV1AcceptedTransitionStatus, PublicV1OperationCommand), String> {
    let revision = operation_revision(accepted, source, &operation_id, &program, "swap")?;
    let intent = PublicOperationIntent {
        identity: accepted.identity.clone(),
        operation_id: operation_id.clone(),
        revision,
        process_session_id: source.report.process_session_id.clone(),
        class: PublicOperationClass::PlannedSwap,
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program),
    };
    intent.validate().map_err(str::to_owned)?;
    let transition = PublicV1AcceptedTransitionStatus {
        resource_version: resource_version.to_string(),
        evidence: accepted.clone(),
        intent: PublicV1AcceptedTransitionIntent {
            consumer: PublicV1EvidenceConsumer::Switchover,
            operation_id,
            revision,
            call_kind: PublicV1CallKind::Swap,
            history: accepted.history.clone(),
            source: PublicV1ResourceAuthority {
                storage_incarnation: source.report.storage_incarnation.clone(),
                resource_version: resource_version.to_string(),
            },
            target: None,
            process_session_id: source.report.process_session_id.clone(),
            input_digest: intent.input_digest.clone(),
            replay_disposition: intent.replay_disposition,
            previous_configuration_id: None,
            current_configuration_id: Some(next.configuration_id.clone()),
            catch_up_mode: Some(PublicCatchUpMode::All),
            catch_up_specific_quorum: Some(false),
        },
    };
    transition.validate()?;
    Ok((
        transition,
        PublicV1OperationCommand {
            target: source.report.replica.clone(),
            intent,
        },
    ))
}

fn planned_swap_program(
    current: &ConfigurationDescriptor,
    next: &ConfigurationDescriptor,
    replicas: &[PublicV1TopologyReplica],
    source: &PublicV1TopologyReplica,
) -> Result<PublicOperationProgram, String> {
    Ok(PublicOperationProgram::Swap {
        starting: public_configuration(&source.report.replica, replicas, current)?,
        refreshed: public_configuration(&source.report.replica, replicas, next)?,
        epoch: next.epoch,
        handoff: ReplicaRole::ActiveSecondary,
        catch_up_specific_quorum: false,
        mode: PublicCatchUpMode::All,
        controller_composed: true,
    })
}

fn content_addressed_operation_id(prefix: String, program: &PublicOperationProgram) -> OperationId {
    OperationId::new(format!("{prefix}-{}", program.digest().sha256()))
}

fn choose_primary(
    current: Option<&ConfigurationDescriptor>,
    replicas: &[PublicV1TopologyReplica],
    switchover_target: Option<ReplicaId>,
) -> Result<ReplicaId, String> {
    if let Some(target) = switchover_target {
        if replicas
            .iter()
            .any(|replica| replica.report.replica.replica_id == target)
        {
            return Ok(target);
        }
        return Err("planned switchover target is unavailable".into());
    }
    if let Some(current) = current
        && replicas.iter().any(|replica| {
            current.members.iter().any(|member| {
                member.role == ReplicaRole::Primary && member.identity == replica.report.replica
            })
        })
    {
        return Ok(current.primary_id);
    }
    let promoted = replicas
        .iter()
        .filter(|replica| replica.report.role == ReplicaRole::Primary)
        .map(|replica| replica.report.replica.replica_id)
        .collect::<Vec<_>>();
    match promoted.as_slice() {
        [primary] => return Ok(*primary),
        [] => {}
        _ => return Err("public-v1 topology observed multiple promoted primaries".into()),
    }
    replicas
        .iter()
        .max_by(|left, right| {
            left.report
                .history
                .compare_progress(
                    left.report.progress.last,
                    &right.report.history,
                    right.report.progress.last,
                )
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    right
                        .report
                        .replica
                        .replica_id
                        .cmp(&left.report.replica.replica_id)
                })
        })
        .map(|replica| replica.report.replica.replica_id)
        .ok_or_else(|| "public-v1 topology has no eligible replica".into())
}

fn desired_configuration(
    current: Option<&ConfigurationDescriptor>,
    replicas: &[PublicV1TopologyReplica],
    primary_id: ReplicaId,
) -> Result<ConfigurationDescriptor, String> {
    let members = replicas
        .iter()
        .map(|replica| ConfigurationMember {
            identity: replica.report.replica.clone(),
            role: if replica.report.replica.replica_id == primary_id {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            },
        })
        .collect::<Vec<_>>();
    let unchanged = current.is_some_and(|configuration| {
        configuration.primary_id == primary_id
            && configuration.members == members
            && configuration.write_quorum == majority(members.len())
    });
    let epoch = if unchanged {
        current.expect("checked").epoch
    } else if let Some(current) = current {
        Epoch::new(
            current.epoch.data_loss_number,
            current.epoch.configuration_number + 1,
        )
    } else {
        Epoch::new(0, 1)
    };
    Ok(ConfigurationDescriptor::new(
        epoch,
        primary_id,
        members,
        majority(replicas.len()),
    ))
}

fn majority(count: usize) -> u32 {
    (count as u32) / 2 + 1
}

fn configuration_is_admitted(
    configuration: &ConfigurationDescriptor,
    replicas: &[PublicV1TopologyReplica],
) -> bool {
    configuration.members.iter().all(|member| {
        replicas.iter().any(|replica| {
            replica.report.replica == member.identity
                && !replica.replication_address.is_empty()
                && replica.report.role == member.role
                && match member.role {
                    ReplicaRole::Primary => {
                        replica.report.write_status
                            == kuberic_runtime::protocol::types::AccessStatus::Granted
                    }
                    ReplicaRole::ActiveSecondary => {
                        replica.report.read_status
                            == kuberic_runtime::protocol::types::AccessStatus::Granted
                    }
                    _ => false,
                }
        })
    })
}

fn operation_sequence(
    local: &PublicV1TopologyReplica,
    replicas: &[PublicV1TopologyReplica],
    configuration: &ConfigurationDescriptor,
    prebuilt: bool,
    reopen_retained: bool,
    catch_up_primary: bool,
) -> Result<Vec<(&'static str, PublicOperationProgram)>, String> {
    let role = configuration
        .members
        .iter()
        .find(|member| member.identity == local.report.replica)
        .map(|member| member.role)
        .ok_or_else(|| "local replica is not in desired configuration".to_string())?;
    let public_configuration =
        public_configuration(&local.report.replica, replicas, configuration)?;
    let open = (
        "open",
        PublicOperationProgram::Open {
            replica: local.report.replica.clone(),
            existing: true,
        },
    );
    let role = (
        "role",
        PublicOperationProgram::Role {
            epoch: configuration.epoch,
            role,
        },
    );
    let epoch = (
        "epoch",
        PublicOperationProgram::Epoch {
            epoch: configuration.epoch,
        },
    );
    let configuration = if catch_up_primary {
        (
            "catch-up",
            PublicOperationProgram::CatchUp {
                configuration: public_configuration,
                mode: PublicCatchUpMode::All,
            },
        )
    } else {
        (
            "configuration",
            PublicOperationProgram::Configuration(public_configuration),
        )
    };
    if should_reopen(local.report.role, prebuilt, reopen_retained) {
        Ok(vec![open, role, epoch, configuration])
    } else if reopen_retained {
        Ok(vec![open, epoch, role, configuration])
    } else {
        Ok(vec![epoch, role, configuration])
    }
}

fn public_configuration(
    local: &kuberic_runtime::protocol::types::ReplicaIdentity,
    replicas: &[PublicV1TopologyReplica],
    configuration: &ConfigurationDescriptor,
) -> Result<PublicConfiguration, String> {
    let peers = configuration
        .members
        .iter()
        .filter(|member| member.identity != *local)
        .map(|member| {
            let replica = replicas
                .iter()
                .find(|replica| replica.report.replica == member.identity)
                .ok_or_else(|| {
                    format!(
                        "public-v1 configuration member {} lacks exact observed authority",
                        member.identity.replica_id
                    )
                })?;
            Ok(PublicReplicaDescriptor {
                build_id: OperationId::default(),
                identity: replica.report.replica.clone(),
                replication_address: replica.replication_address.clone(),
                process_session_id: replica.report.process_session_id.clone(),
                storage_incarnation: Some(replica.report.storage_incarnation.clone()),
                role: member.role,
                current_progress: replica.report.progress.last,
                catch_up_capability: replica.report.progress.first,
                must_catchup: false,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let configuration = PublicConfiguration {
        current: configuration.clone(),
        previous: None,
        replicas: peers,
    };
    configuration.validate().map_err(str::to_owned)?;
    Ok(configuration)
}

fn should_reopen(role: ReplicaRole, prebuilt: bool, reopen_retained: bool) -> bool {
    reopen_retained && role == ReplicaRole::None && !prebuilt
}

fn begin_operation(
    accepted: &PublicV1AcceptedStatus,
    replica: &PublicV1TopologyReplica,
    program: PublicOperationProgram,
    operation_id: OperationId,
    resource_version: &str,
) -> Result<(PublicV1AcceptedTransitionStatus, PublicV1OperationCommand), String> {
    let revision = operation_revision(accepted, replica, &operation_id, &program, "operation")?;
    let class = match &program {
        PublicOperationProgram::Build(build) => PublicOperationClass::Build {
            target: build.replica.replica_id,
        },
        PublicOperationProgram::Remove(build) => PublicOperationClass::Remove {
            target: build.replica.replica_id,
        },
        PublicOperationProgram::Swap { .. } => PublicOperationClass::PlannedSwap,
        PublicOperationProgram::Close => PublicOperationClass::Close,
        PublicOperationProgram::Abort => PublicOperationClass::Abort,
        _ => PublicOperationClass::Authority,
    };
    let intent = PublicOperationIntent {
        identity: accepted.identity.clone(),
        operation_id: operation_id.clone(),
        revision,
        process_session_id: replica.report.process_session_id.clone(),
        class,
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program),
    };
    intent.validate().map_err(str::to_owned)?;
    let call_kind = call_kind(&intent)?;
    let (previous_configuration_id, current_configuration_id) = configuration_ids(&intent);
    let (catch_up_mode, catch_up_specific_quorum) = match intent.program.as_ref() {
        Some(PublicOperationProgram::CatchUp { mode, .. }) => (Some(*mode), None),
        _ => (None, None),
    };
    let transition = PublicV1AcceptedTransitionStatus {
        resource_version: resource_version.to_string(),
        evidence: accepted.clone(),
        intent: PublicV1AcceptedTransitionIntent {
            consumer: PublicV1EvidenceConsumer::Topology,
            operation_id,
            revision,
            call_kind,
            history: accepted.history.clone(),
            source: PublicV1ResourceAuthority {
                storage_incarnation: replica.report.storage_incarnation.clone(),
                resource_version: resource_version.to_string(),
            },
            target: None,
            process_session_id: replica.report.process_session_id.clone(),
            input_digest: intent.input_digest.clone(),
            replay_disposition: intent.replay_disposition,
            previous_configuration_id,
            current_configuration_id,
            catch_up_mode,
            catch_up_specific_quorum,
        },
    };
    transition.validate()?;
    Ok((
        transition,
        PublicV1OperationCommand {
            target: replica.report.replica.clone(),
            intent,
        },
    ))
}

fn operation_revision(
    accepted: &PublicV1AcceptedStatus,
    replica: &PublicV1TopologyReplica,
    operation_id: &OperationId,
    program: &PublicOperationProgram,
    kind: &str,
) -> Result<u64, String> {
    let digest = program.digest();
    if let Some(completion) = replica
        .report
        .completed_operations
        .iter()
        .find(|completion| {
            completion.operation_id == *operation_id
                && completion.process_session_id == replica.report.process_session_id
                && completion.input_digest == digest
                && completion.replay_disposition == program.replay_disposition()
                && completion.history == accepted.history
                && completion.storage_incarnation == replica.report.storage_incarnation
                && completion.result == PublicV1OperationResult::Succeeded
        })
    {
        return Ok(completion.revision);
    }
    replica
        .report
        .completed_operations
        .iter()
        .map(|completion| completion.revision)
        .chain(
            accepted
                .completed_operations
                .iter()
                .filter(|completion| {
                    completion.storage_incarnation.replica == replica.report.replica
                })
                .map(|completion| completion.revision),
        )
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| format!("public-v1 {kind} revision overflow"))
}

fn retained_authority_revision(
    accepted: &PublicV1AcceptedStatus,
    replica: &PublicV1TopologyReplica,
    kind: &str,
) -> Result<u64, String> {
    replica
        .report
        .completed_operations
        .iter()
        .map(|completion| completion.revision)
        .chain(
            accepted
                .completed_operations
                .iter()
                .filter(|completion| {
                    completion.storage_incarnation.replica == replica.report.replica
                })
                .map(|completion| completion.revision),
        )
        .max()
        .filter(|revision| *revision > 0)
        .ok_or_else(|| format!("public-v1 {kind} lacks retained source authority"))
}

fn call_kind(intent: &PublicOperationIntent) -> Result<PublicV1CallKind, String> {
    match intent.program.as_ref() {
        Some(PublicOperationProgram::Open { .. }) => Ok(PublicV1CallKind::Open),
        Some(PublicOperationProgram::Role { .. }) => Ok(PublicV1CallKind::Role),
        Some(PublicOperationProgram::Epoch { .. }) => Ok(PublicV1CallKind::Epoch),
        Some(PublicOperationProgram::Configuration(_)) => Ok(PublicV1CallKind::Configuration),
        Some(PublicOperationProgram::CatchUp { .. }) => Ok(PublicV1CallKind::CatchUp),
        Some(PublicOperationProgram::Progress { .. }) => Ok(PublicV1CallKind::Progress),
        Some(PublicOperationProgram::Swap { .. }) => Ok(PublicV1CallKind::Swap),
        Some(PublicOperationProgram::Build(_)) => Ok(PublicV1CallKind::Build),
        Some(PublicOperationProgram::Remove(_)) => Ok(PublicV1CallKind::Remove),
        Some(PublicOperationProgram::Close) => Ok(PublicV1CallKind::Close),
        Some(PublicOperationProgram::Abort) => Ok(PublicV1CallKind::Abort),
        None => Err("public-v1 topology intent lacks a program".into()),
    }
}

fn configuration_ids(
    intent: &PublicOperationIntent,
) -> (Option<ConfigurationId>, Option<ConfigurationId>) {
    match intent.program.as_ref() {
        Some(PublicOperationProgram::Configuration(configuration))
        | Some(PublicOperationProgram::CatchUp { configuration, .. }) => (
            configuration
                .previous
                .as_ref()
                .map(|previous| previous.configuration_id.clone()),
            Some(configuration.current.configuration_id.clone()),
        ),
        _ => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kuberic_runtime::protocol::public_operations::{
        PublicProgress, PublicProgressRole, PublicV1AcceptedOperationCompletion,
        PublicV1AcceptedStatus, PublicV1CallKind, PublicV1Extensions, PublicV1Identity,
        PublicV1OperationCompletion, PublicV1OperationResult,
    };
    use kuberic_runtime::protocol::types::{
        AccessStatus, AgentGeneration, PodUid, ProcessSessionId, PublicProtocolGeneration, PvcUid,
        ReplicaIdentity, ReplicaInstanceId, ReplicaStorageIdentity,
        ReplicaStorageIncarnationContext, ReplicatorCreationId, ResourceUid, StorageGeneration,
        derive_bootstrap_history, derive_history_initialization_id,
    };

    fn replica(id: i64, role: ReplicaRole) -> PublicV1TopologyReplica {
        let resource_uid = ResourceUid::new("resource-topology-test");
        let generation = PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(id),
            instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
            agent_generation: AgentGeneration::new(format!("generation-{id}")),
        };
        PublicV1TopologyReplica {
            report: PublicV1AgentReport {
                identity: PublicV1Identity::new(1),
                resource_uid: resource_uid.clone(),
                replica: identity.clone(),
                process_session_id: ProcessSessionId::new(format!("session-{id}")),
                replicator_creation_id: ReplicatorCreationId::new(format!("creation-{id}")),
                report_sequence: 1,
                role,
                read_status: if role == ReplicaRole::None {
                    AccessStatus::NotPrimary
                } else {
                    AccessStatus::Granted
                },
                write_status: if role == ReplicaRole::Primary {
                    AccessStatus::Granted
                } else {
                    AccessStatus::NotPrimary
                },
                history: derive_bootstrap_history(&resource_uid, generation, &initialization),
                storage_incarnation: ReplicaStorageIncarnationContext {
                    replica: identity,
                    storage: ReplicaStorageIdentity::Persistent {
                        pod_uid: PodUid::new(format!("pod-{id}")),
                        pvc_uid: PvcUid::new(format!("pvc-{id}")),
                    },
                    storage_generation: StorageGeneration::new(format!("storage-{id}")),
                },
                progress: PublicProgress {
                    role: match role {
                        ReplicaRole::Primary => PublicProgressRole::Primary,
                        ReplicaRole::ActiveSecondary | ReplicaRole::IdleSecondary => {
                            PublicProgressRole::Secondary
                        }
                        ReplicaRole::None => PublicProgressRole::Recovery,
                    },
                    first: 0,
                    last: 0,
                },
                replication_address: Some(format!("http://10.0.0.{id}:50052")),
                service_location: None,
                data_loss_recovery: None,
                completed_operations: Vec::new(),
                extensions: PublicV1Extensions::default(),
            },
            replication_address: format!("http://10.0.0.{id}:50052"),
        }
    }

    fn accepted(replica: &PublicV1TopologyReplica) -> PublicV1AcceptedStatus {
        PublicV1AcceptedStatus {
            identity: replica.report.identity.clone(),
            history_initialization_id: derive_history_initialization_id(
                &replica.report.resource_uid,
                PublicProtocolGeneration::new(replica.report.identity.generation),
            ),
            history: replica.report.history.clone(),
            data_loss_recovery: None,
            completed_operations: Vec::new(),
            extensions: PublicV1Extensions::default(),
        }
    }

    fn accept_command(
        accepted: &mut PublicV1AcceptedStatus,
        replicas: &mut [PublicV1TopologyReplica],
        command: &PublicV1OperationCommand,
    ) {
        let report = replicas
            .iter_mut()
            .find(|replica| replica.report.replica == command.target)
            .expect("command target");
        let (
            call_kind,
            previous_configuration_id,
            current_configuration_id,
            catch_up_mode,
            catch_up_specific_quorum,
        ) = match command.intent.program.as_ref().expect("program") {
            PublicOperationProgram::Open { .. } => (PublicV1CallKind::Open, None, None, None, None),
            PublicOperationProgram::Role { role, .. } => {
                report.report.role = if *role == ReplicaRole::IdleSecondary {
                    ReplicaRole::None
                } else {
                    *role
                };
                report.report.progress.role = match report.report.role {
                    ReplicaRole::Primary => PublicProgressRole::Primary,
                    ReplicaRole::ActiveSecondary => PublicProgressRole::Secondary,
                    ReplicaRole::IdleSecondary | ReplicaRole::None => PublicProgressRole::Recovery,
                };
                report.report.read_status = AccessStatus::NotPrimary;
                report.report.write_status = AccessStatus::NotPrimary;
                (PublicV1CallKind::Role, None, None, None, None)
            }
            PublicOperationProgram::Epoch { .. } => {
                (PublicV1CallKind::Epoch, None, None, None, None)
            }
            PublicOperationProgram::Configuration(configuration) => {
                report.report.read_status = AccessStatus::Granted;
                report.report.write_status = if report.report.role == ReplicaRole::Primary {
                    AccessStatus::Granted
                } else {
                    AccessStatus::NotPrimary
                };
                (
                    PublicV1CallKind::Configuration,
                    configuration
                        .previous
                        .as_ref()
                        .map(|configuration| configuration.configuration_id.clone()),
                    Some(configuration.current.configuration_id.clone()),
                    None,
                    None,
                )
            }
            PublicOperationProgram::CatchUp {
                configuration,
                mode,
            } => {
                report.report.read_status = AccessStatus::Granted;
                report.report.write_status = AccessStatus::Granted;
                (
                    PublicV1CallKind::CatchUp,
                    configuration
                        .previous
                        .as_ref()
                        .map(|configuration| configuration.configuration_id.clone()),
                    Some(configuration.current.configuration_id.clone()),
                    Some(*mode),
                    None,
                )
            }
            other => panic!("unexpected preparation program: {other:?}"),
        };
        let completion = PublicV1OperationCompletion {
            operation_id: command.intent.operation_id.clone(),
            revision: command.intent.revision,
            call_kind,
            process_session_id: command.intent.process_session_id.clone(),
            input_digest: command.intent.input_digest.clone(),
            replay_disposition: command.intent.replay_disposition,
            history: accepted.history.clone(),
            storage_incarnation: report.report.storage_incarnation.clone(),
            previous_configuration_id,
            current_configuration_id,
            catch_up_mode,
            catch_up_specific_quorum,
            result: PublicV1OperationResult::Succeeded,
        };
        accepted
            .completed_operations
            .push(PublicV1AcceptedOperationCompletion::from_completion(
                &completion,
            ));
    }

    #[test]
    fn ordinary_failover_does_not_reopen_transiently_fenced_replica() {
        assert!(!should_reopen(ReplicaRole::None, false, false));
    }

    #[test]
    fn bootstrap_opens_without_endpoints_then_waits_for_all_open_results() {
        let mut replicas = vec![
            replica(1, ReplicaRole::None),
            replica(2, ReplicaRole::None),
            replica(3, ReplicaRole::None),
        ];
        for replica in &mut replicas {
            replica.replication_address.clear();
            replica.report.replication_address = None;
        }
        let mut accepted = accepted(&replicas[0]);
        for index in 0..replicas.len() {
            let PublicV1TopologyPlan::Begin { command, .. } = plan_public_v1_topology(
                &accepted,
                None,
                &replicas,
                3,
                None,
                None,
                "resource-version",
            )
            .unwrap() else {
                panic!("bootstrap must authorize Open before discovering its endpoint");
            };
            assert_eq!(command.target, replicas[index].report.replica);
            assert!(matches!(
                command.intent.program,
                Some(PublicOperationProgram::Open { .. })
            ));
            accept_command(&mut accepted, &mut replicas, &command);
            assert_eq!(
                plan_public_v1_topology(
                    &accepted,
                    None,
                    &replicas,
                    3,
                    None,
                    None,
                    "resource-version",
                )
                .unwrap(),
                PublicV1TopologyPlan::Wait,
            );
            let address = format!("http://127.0.0.1:{}", 41001 + index);
            replicas[index].replication_address = address.clone();
            replicas[index].report.replication_address = Some(address);
        }
        for _ in 0..20 {
            let plan = plan_public_v1_topology(
                &accepted,
                None,
                &replicas,
                3,
                None,
                None,
                "resource-version",
            )
            .unwrap();
            match plan {
                PublicV1TopologyPlan::Begin { command, .. } => {
                    assert!(
                        !matches!(
                            command.intent.program,
                            Some(PublicOperationProgram::Open { .. })
                        ),
                        "Open discovery and topology preparation must share the same operation ID"
                    );
                    if let Some(PublicOperationProgram::Configuration(configuration)) =
                        command.intent.program.as_ref()
                    {
                        for peer in &configuration.replicas {
                            let exact = replicas
                                .iter()
                                .find(|replica| replica.report.replica == peer.identity)
                                .unwrap();
                            assert_eq!(
                                Some(&peer.replication_address),
                                exact.report.replication_address.as_ref()
                            );
                        }
                    }
                    accept_command(&mut accepted, &mut replicas, &command);
                }
                PublicV1TopologyPlan::PersistTopology { .. } => return,
                other => panic!("bootstrap did not progress after endpoint discovery: {other:?}"),
            }
        }
        panic!("bootstrap did not admit the opened replicas");
    }

    #[test]
    fn public_configuration_uses_exact_members_and_rejects_missing_authority() {
        let replicas = vec![
            replica(1, ReplicaRole::Primary),
            replica(2, ReplicaRole::ActiveSecondary),
            replica(3, ReplicaRole::ActiveSecondary),
        ];
        let configuration = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            ReplicaId::new(1),
            vec![
                ConfigurationMember {
                    identity: replicas[0].report.replica.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: replicas[1].report.replica.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );

        let exact =
            public_configuration(&replicas[0].report.replica, &replicas, &configuration).unwrap();
        assert_eq!(exact.replicas.len(), 1);
        assert_eq!(exact.replicas[0].identity, replicas[1].report.replica);
        assert!(
            public_configuration(&replicas[0].report.replica, &replicas[..1], &configuration)
                .is_err()
        );
    }

    #[test]
    fn corrective_primary_promotes_survivor_before_replacement_build() {
        let old_primary = replica(1, ReplicaRole::Primary);
        let mut fresh_primary = replica(1, ReplicaRole::None);
        fresh_primary.report.replica.instance_id = ReplicaInstanceId::new("pod-1-fresh");
        fresh_primary.report.replica.agent_generation = AgentGeneration::new("generation-1-fresh");
        fresh_primary.report.process_session_id = ProcessSessionId::new("session-1-fresh");
        fresh_primary.report.storage_incarnation.replica = fresh_primary.report.replica.clone();
        fresh_primary.report.storage_incarnation.storage = ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("pod-1-fresh"),
            pvc_uid: PvcUid::new("pvc-1-fresh"),
        };
        fresh_primary.report.storage_incarnation.storage_generation =
            StorageGeneration::new("storage-1-fresh");
        let second = replica(2, ReplicaRole::ActiveSecondary);
        let third = replica(3, ReplicaRole::ActiveSecondary);
        let mut current = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            ReplicaId::new(1),
            vec![
                ConfigurationMember {
                    identity: old_primary.report.replica.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: second.report.replica.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: third.report.replica.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        let mut accepted = accepted(&old_primary);
        let mut replicas = vec![fresh_primary, second, third];
        let mut routed_primary = Some(ReplicaId::new(1));

        for _ in 0..40 {
            match plan_public_v1_topology(
                &accepted,
                Some(&current),
                &replicas,
                3,
                None,
                routed_primary,
                "resource-version",
            )
            .unwrap()
            {
                PublicV1TopologyPlan::Begin { command, .. } => {
                    if let Some(PublicOperationProgram::Build(build)) =
                        command.intent.program.as_ref()
                    {
                        assert_ne!(current.primary_id, ReplicaId::new(1));
                        assert_eq!(build.epoch, current.epoch);
                        assert_eq!(
                            build.current_configuration_id.as_ref(),
                            Some(&current.configuration_id)
                        );
                        assert_eq!(command.target.replica_id, current.primary_id);
                        return;
                    }
                    accept_command(&mut accepted, &mut replicas, &command);
                }
                PublicV1TopologyPlan::RoutePrimary { primary } => {
                    assert_ne!(primary.replica_id, ReplicaId::new(1));
                    routed_primary = Some(primary.replica_id);
                }
                PublicV1TopologyPlan::PersistTopology { expected, next } => {
                    assert_eq!(expected.as_ref(), Some(&current.configuration_id));
                    current = *next;
                }
                PublicV1TopologyPlan::Wait => {
                    panic!("corrective primary waited before topology persistence or Build")
                }
                other => panic!("unexpected corrective-primary plan: {other:?}"),
            }
        }
        panic!("corrective primary never reached a configuration-consistent Build");
    }
}
