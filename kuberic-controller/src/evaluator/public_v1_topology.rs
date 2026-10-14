use kuberic_runtime::protocol::command::PublicV1OperationCommand;
use kuberic_runtime::protocol::public_operations::{
    PublicBuildInput, PublicCatchUpMode, PublicConfiguration, PublicOperationClass,
    PublicOperationIntent, PublicOperationProgram, PublicReplicaDescriptor, PublicV1AcceptedResult,
    PublicV1AcceptedStatus, PublicV1AgentReport, PublicV1CallKind,
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
                && !replica.replication_address.is_empty()
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
    if let Some(current) = current
        && current.primary_id != operational_next.primary_id
        && let Some(source) = observed
            .iter()
            .find(|replica| replica.report.replica.replica_id == current.primary_id)
    {
        let catch_up_id = OperationId::new(format!(
            "topology-{}-catch-up-{}",
            current.configuration_id, operational_next.primary_id
        ));
        if !accepted.completed_operations.iter().any(|completion| {
            completion.operation_id == catch_up_id
                && completion.result == PublicV1AcceptedResult::Succeeded
        }) {
            return begin_operation(
                accepted,
                source,
                PublicOperationProgram::CatchUp {
                    configuration: public_configuration(
                        &source.report.replica,
                        &observed,
                        current,
                    )?,
                    mode: PublicCatchUpMode::All,
                },
                catch_up_id,
                resource_version,
            )
            .map(|(transition, command)| PublicV1TopologyPlan::Begin {
                transition: Box::new(transition),
                command: Box::new(command),
            });
        }
        let swap_id = OperationId::new(format!(
            "topology-{}-swap-{}",
            current.configuration_id, operational_next.primary_id
        ));
        if !accepted.completed_operations.iter().any(|completion| {
            completion.operation_id == swap_id
                && completion.result == PublicV1AcceptedResult::Succeeded
        }) {
            return begin_swap(
                accepted,
                current,
                operational_next,
                &observed,
                source,
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
        let source = eligible
            .iter()
            .find(|replica| replica.report.replica.replica_id == current.primary_id)
            .or_else(|| {
                eligible
                    .iter()
                    .find(|replica| replica.report.replica.replica_id == primary_id)
            })
            .ok_or_else(|| "public-v1 build lacks an available source".to_string())?;
        let build_id = OperationId::new(format!(
            "topology-{}-build-{}",
            current.configuration_id, target.report.replica.replica_id
        ));
        if !accepted.completed_operations.iter().any(|completion| {
            completion.operation_id == build_id
                && completion.result == PublicV1AcceptedResult::Succeeded
        }) {
            return begin_build(
                accepted,
                current,
                &next,
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
    for replica in operational_replicas {
        for (step, program) in operation_sequence(replica, operational_replicas, operational_next)?
        {
            let operation_id = OperationId::new(format!(
                "topology-{}-{}-{}-{}",
                operational_next.configuration_id,
                replica.report.replica.replica_id,
                replica.report.process_session_id,
                step
            ));
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
            "topology-{}-remove-{}",
            current.configuration_id, target.report.replica.replica_id
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
        let revision = source
            .report
            .completed_operations
            .iter()
            .map(|completion| completion.revision)
            .chain(
                accepted
                    .completed_operations
                    .iter()
                    .filter(|completion| {
                        completion.storage_incarnation.replica == source.report.replica
                    })
                    .map(|completion| completion.revision),
            )
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| "public-v1 removal revision overflow".to_string())?;
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
    next: &ConfigurationDescriptor,
    source: &PublicV1TopologyReplica,
    target: &PublicV1TopologyReplica,
    operation_id: OperationId,
    resource_version: &str,
) -> Result<(PublicV1AcceptedTransitionStatus, PublicV1OperationCommand), String> {
    let revision = source
        .report
        .completed_operations
        .iter()
        .map(|completion| completion.revision)
        .chain(
            accepted
                .completed_operations
                .iter()
                .filter(|completion| {
                    completion.storage_incarnation.replica == source.report.replica
                })
                .map(|completion| completion.revision),
        )
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| "public-v1 build revision overflow".to_string())?;
    let build = PublicBuildInput {
        attempt: operation_id.clone(),
        epoch: next.epoch,
        current_configuration_id: Some(current.configuration_id.clone()),
        replica: target.report.replica.clone(),
        target_storage: target.report.storage_incarnation.clone(),
        process_session_id: target.report.process_session_id.clone(),
        replication_address: target.replication_address.clone(),
        history: Some(accepted.history.clone()),
    };
    let program = PublicOperationProgram::Build(build);
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
    current: &ConfigurationDescriptor,
    next: &ConfigurationDescriptor,
    replicas: &[PublicV1TopologyReplica],
    source: &PublicV1TopologyReplica,
    operation_id: OperationId,
    resource_version: &str,
) -> Result<(PublicV1AcceptedTransitionStatus, PublicV1OperationCommand), String> {
    let revision = source
        .report
        .completed_operations
        .iter()
        .map(|completion| completion.revision)
        .chain(
            accepted
                .completed_operations
                .iter()
                .filter(|completion| {
                    completion.storage_incarnation.replica == source.report.replica
                })
                .map(|completion| completion.revision),
        )
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| "public-v1 swap revision overflow".to_string())?;
    let program = PublicOperationProgram::Swap {
        starting: public_configuration(&source.report.replica, replicas, current)?,
        refreshed: public_configuration(&source.report.replica, replicas, next)?,
        epoch: next.epoch,
        handoff: ReplicaRole::ActiveSecondary,
        catch_up_specific_quorum: false,
        mode: PublicCatchUpMode::All,
        controller_composed: true,
    };
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
        && replicas
            .iter()
            .any(|replica| replica.report.replica.replica_id == current.primary_id)
    {
        return Ok(current.primary_id);
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

fn operation_sequence(
    local: &PublicV1TopologyReplica,
    replicas: &[PublicV1TopologyReplica],
    configuration: &ConfigurationDescriptor,
) -> Result<Vec<(&'static str, PublicOperationProgram)>, String> {
    let role = configuration
        .members
        .iter()
        .find(|member| member.identity == local.report.replica)
        .map(|member| member.role)
        .ok_or_else(|| "local replica is not in desired configuration".to_string())?;
    let public_configuration =
        public_configuration(&local.report.replica, replicas, configuration)?;
    Ok(vec![
        (
            "open",
            PublicOperationProgram::Open {
                replica: local.report.replica.clone(),
                existing: true,
            },
        ),
        (
            "role",
            PublicOperationProgram::Role {
                epoch: configuration.epoch,
                role,
            },
        ),
        (
            "epoch",
            PublicOperationProgram::Epoch {
                epoch: configuration.epoch,
            },
        ),
        (
            "configuration",
            PublicOperationProgram::Configuration(public_configuration),
        ),
    ])
}

fn public_configuration(
    local: &kuberic_runtime::protocol::types::ReplicaIdentity,
    replicas: &[PublicV1TopologyReplica],
    configuration: &ConfigurationDescriptor,
) -> Result<PublicConfiguration, String> {
    let peers = replicas
        .iter()
        .filter(|replica| replica.report.replica != *local)
        .map(|replica| PublicReplicaDescriptor {
            build_id: OperationId::default(),
            identity: replica.report.replica.clone(),
            replication_address: replica.replication_address.clone(),
            process_session_id: replica.report.process_session_id.clone(),
            storage_incarnation: Some(replica.report.storage_incarnation.clone()),
            role: configuration
                .members
                .iter()
                .find(|member| member.identity == replica.report.replica)
                .map(|member| member.role)
                .expect("eligible replica is configured"),
            current_progress: replica.report.progress.last,
            catch_up_capability: replica.report.progress.first,
            must_catchup: false,
        })
        .collect();
    let configuration = PublicConfiguration {
        current: configuration.clone(),
        previous: None,
        replicas: peers,
    };
    configuration.validate().map_err(str::to_owned)?;
    Ok(configuration)
}

fn begin_operation(
    accepted: &PublicV1AcceptedStatus,
    replica: &PublicV1TopologyReplica,
    program: PublicOperationProgram,
    operation_id: OperationId,
    resource_version: &str,
) -> Result<(PublicV1AcceptedTransitionStatus, PublicV1OperationCommand), String> {
    let revision = replica
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
        .ok_or_else(|| "public-v1 operation revision overflow".to_string())?;
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
