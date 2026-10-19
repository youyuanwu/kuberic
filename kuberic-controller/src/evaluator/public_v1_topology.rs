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
    if current == Some(&next)
        && eligible
            .iter()
            .all(|replica| !replica.replication_address.is_empty())
        && let Some(primary) = eligible.iter().find(|replica| {
            replica.report.replica.replica_id == next.primary_id
                && replica
                    .report
                    .completed_operations
                    .iter()
                    .any(|completion| {
                        completion.process_session_id == replica.report.process_session_id
                            && completion.current_configuration_id.as_ref()
                                == Some(&next.configuration_id)
                            && matches!(
                                completion.call_kind,
                                PublicV1CallKind::Configuration | PublicV1CallKind::CatchUp
                            )
                            && completion.result == PublicV1OperationResult::Succeeded
                    })
        })
    {
        let program = PublicOperationProgram::CatchUp {
            configuration: public_configuration(&primary.report.replica, &eligible, &next)?,
            mode: PublicCatchUpMode::All,
        };
        let operation_id = content_addressed_operation_id(
            format!(
                "topology-{}-recover-{}",
                next.configuration_id, primary.report.process_session_id
            ),
            &program,
        );
        if !accepted.completed_operations.iter().any(|completion| {
            completion.operation_id == operation_id
                && completion.result == PublicV1AcceptedResult::Succeeded
        }) {
            return begin_operation(accepted, primary, program, operation_id, resource_version)
                .map(|(transition, command)| PublicV1TopologyPlan::Begin {
                    transition: Box::new(transition),
                    command: Box::new(command),
                });
        }
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
    let retained_rejoins = eligible
        .iter()
        .filter(|target| {
            current.is_some_and(|configuration| {
                !configuration
                    .members
                    .iter()
                    .any(|member| member.identity == target.report.replica)
                    && can_rejoin_retained_member(accepted, configuration, &eligible, target)
            })
        })
        .map(|target| target.report.replica.clone())
        .collect::<Vec<_>>();
    if let Some(replica) = operational_replicas.iter().find(|replica| {
        replica.replication_address.is_empty() || !replica.report.has_current_open_endpoint()
    }) {
        let open = PublicOperationProgram::Open {
            replica: replica.report.replica.clone(),
            existing: replica.report.open_mode == kuberic_runtime::application::OpenMode::Existing,
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
        let open_base = format!(
            "topology-{}-{}-{}-open",
            open_configuration_id,
            replica.report.replica.replica_id,
            replica.report.process_session_id,
        );
        // Initialization changes the reported mode, not exact successful Open credit.
        if [false, true].into_iter().any(|existing| {
            let program = PublicOperationProgram::Open {
                replica: replica.report.replica.clone(),
                existing,
            };
            let operation_id = content_addressed_operation_id(open_base.clone(), &program);
            let digest = program.digest();
            accepted.completed_operations.iter().any(|completion| {
                completion.operation_id == operation_id
                    && completion.call_kind == PublicV1CallKind::Open
                    && completion.process_session_id == replica.report.process_session_id
                    && completion.storage_incarnation == replica.report.storage_incarnation
                    && completion.history == replica.report.history
                    && completion.input_digest_version == digest.version()
                    && completion.input_digest_sha256 == digest.sha256()
                    && completion.result == PublicV1AcceptedResult::Succeeded
            })
        }) {
            return Ok(PublicV1TopologyPlan::Wait);
        }
        let open_id = content_addressed_operation_id(open_base, &open);
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
        let intermediate = ConfigurationDescriptor::new(
            operational_next.epoch,
            current.primary_id,
            operational_next
                .members
                .iter()
                .cloned()
                .map(|mut member| {
                    member.role = if member.identity.replica_id == current.primary_id {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    };
                    member
                })
                .collect(),
            operational_next.write_quorum,
        );
        let base = format!(
            "topology-{}-transfer-{}-{}",
            current.configuration_id,
            operational_next.configuration_id,
            source.report.process_session_id
        );
        let handoff_prefix = format!("{base}-handoff-");
        if !stage_succeeded(
            accepted,
            source,
            &handoff_prefix,
            PublicV1CallKind::Swap,
            Some(&intermediate.configuration_id),
        ) {
            let first_prefix = format!("{base}-first-catch-up-");
            if !stage_succeeded(
                accepted,
                source,
                &first_prefix,
                PublicV1CallKind::CatchUp,
                Some(&current.configuration_id),
            ) {
                let program = PublicOperationProgram::CatchUp {
                    configuration: public_configuration(
                        &source.report.replica,
                        &observed,
                        current,
                    )?,
                    mode: PublicCatchUpMode::All,
                };
                return begin_plan(accepted, source, program, &first_prefix, resource_version);
            }
            for peer in operational_replicas
                .iter()
                .filter(|replica| replica.report.replica != source.report.replica)
            {
                for (step, program) in [
                    (
                        "epoch",
                        PublicOperationProgram::Epoch {
                            epoch: intermediate.epoch,
                        },
                    ),
                    (
                        "role",
                        PublicOperationProgram::Role {
                            epoch: intermediate.epoch,
                            role: ReplicaRole::ActiveSecondary,
                        },
                    ),
                    (
                        "configuration",
                        PublicOperationProgram::Configuration(public_configuration(
                            &peer.report.replica,
                            operational_replicas,
                            &intermediate,
                        )?),
                    ),
                ] {
                    let prefix = format!(
                        "{base}-prepare-{}-{}-{step}-",
                        peer.report.replica.replica_id, peer.report.process_session_id
                    );
                    let expected = if step == "configuration" {
                        Some(&intermediate.configuration_id)
                    } else {
                        None
                    };
                    let kind = match step {
                        "epoch" => PublicV1CallKind::Epoch,
                        "role" => PublicV1CallKind::Role,
                        _ => PublicV1CallKind::Configuration,
                    };
                    if !stage_succeeded(accepted, peer, &prefix, kind, expected) {
                        return begin_plan(accepted, peer, program, &prefix, resource_version);
                    }
                }
            }
            let Some(capability) = source.report.extensions.catch_up_specific_quorum else {
                return Ok(PublicV1TopologyPlan::Wait);
            };
            let mut starting =
                public_configuration(&source.report.replica, operational_replicas, current)?;
            let mut refreshed =
                public_configuration(&source.report.replica, operational_replicas, &intermediate)?;
            if capability {
                for configuration in [&mut starting, &mut refreshed] {
                    for peer in &mut configuration.replicas {
                        peer.must_catchup = peer.identity.replica_id == operational_next.primary_id;
                    }
                }
            }
            let program = PublicOperationProgram::Swap {
                starting,
                refreshed,
                epoch: intermediate.epoch,
                handoff: ReplicaRole::ActiveSecondary,
                catch_up_specific_quorum: capability,
                mode: if capability {
                    PublicCatchUpMode::WriteQuorum
                } else {
                    PublicCatchUpMode::All
                },
                controller_composed: true,
            };
            return begin_plan(accepted, source, program, &handoff_prefix, resource_version);
        }
    }
    if let Some(current) = current
        && let Some(target) = eligible.iter().find(|replica| {
            !current
                .members
                .iter()
                .any(|member| member.identity == replica.report.replica)
                && !retained_rejoins.contains(&replica.report.replica)
        })
    {
        let role = (
            "build-role",
            PublicOperationProgram::Role {
                epoch: current.epoch,
                role: ReplicaRole::IdleSecondary,
            },
        );
        let epoch = (
            "build-epoch",
            PublicOperationProgram::Epoch {
                epoch: current.epoch,
            },
        );
        let preparation = [epoch, role];
        for (step, program) in preparation {
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
        let build_id = topology_build_id(current, source, target);
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
    let changing_primary = current
        .is_some_and(|configuration| configuration.primary_id != operational_next.primary_id);
    if changing_primary || !retained_rejoins.is_empty() {
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
            (changing_primary || !retained_rejoins.is_empty())
                && replica.report.replica.replica_id == operational_next.primary_id,
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

fn can_rejoin_retained_member(
    accepted: &PublicV1AcceptedStatus,
    current: &ConfigurationDescriptor,
    replicas: &[PublicV1TopologyReplica],
    target: &PublicV1TopologyReplica,
) -> bool {
    // Rejoin only a clean tail whose exact storage/session configuration result
    // was actually accepted. The new configuration still fences the member and
    // the primary must prove/catch up the retained log before topology admission.
    let report = &target.report;
    report.validate().is_ok()
        && report.history == accepted.history
        && report.progress.first == report.progress.last
        && replicas.iter().any(|source| {
            current.members.iter().any(|member| {
                member.role == ReplicaRole::Primary && member.identity == source.report.replica
            }) && source.report.progress.first >= report.progress.last
                && !accepted.completed_operations.iter().any(|completion| {
                    completion.operation_id == topology_build_id(current, source, target)
                        && completion.result == PublicV1AcceptedResult::Succeeded
                })
        })
        && report.completed_operations.iter().any(|completion| {
            completion.process_session_id == report.process_session_id
                && completion.storage_incarnation == report.storage_incarnation
                && completion.history == accepted.history
                && completion.current_configuration_id.is_some()
                && matches!(
                    completion.call_kind,
                    PublicV1CallKind::Configuration | PublicV1CallKind::CatchUp
                )
                && completion.result == PublicV1OperationResult::Succeeded
                && accepted.completed_operations.contains(
                    &kuberic_runtime::protocol::public_operations::
                        PublicV1AcceptedOperationCompletion::from_completion(completion),
                )
        })
}

fn topology_build_id(
    current: &ConfigurationDescriptor,
    source: &PublicV1TopologyReplica,
    target: &PublicV1TopologyReplica,
) -> OperationId {
    OperationId::new(format!(
        "topology-{}-build-{}-{}-{}-{}",
        current.configuration_id,
        target.report.replica.replica_id,
        source.report.process_session_id,
        target.report.process_session_id,
        target.report.storage_incarnation.storage_generation,
    ))
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

fn content_addressed_operation_id(prefix: String, program: &PublicOperationProgram) -> OperationId {
    OperationId::new(format!("{prefix}-{}", program.digest().sha256()))
}

fn stage_succeeded(
    accepted: &PublicV1AcceptedStatus,
    replica: &PublicV1TopologyReplica,
    prefix: &str,
    kind: PublicV1CallKind,
    configuration: Option<&ConfigurationId>,
) -> bool {
    accepted.completed_operations.iter().any(|completion| {
        completion.operation_id.as_str().starts_with(prefix)
            && completion.call_kind == kind
            && completion.storage_incarnation == replica.report.storage_incarnation
            && completion.process_session_id == replica.report.process_session_id
            && completion.history == accepted.history
            && completion.current_configuration_id.as_ref() == configuration
            && completion.result == PublicV1AcceptedResult::Succeeded
    })
}

fn begin_plan(
    accepted: &PublicV1AcceptedStatus,
    replica: &PublicV1TopologyReplica,
    program: PublicOperationProgram,
    prefix: &str,
    resource_version: &str,
) -> Result<PublicV1TopologyPlan, String> {
    let id = content_addressed_operation_id(prefix.trim_end_matches('-').to_owned(), &program);
    begin_operation(accepted, replica, program, id, resource_version).map(
        |(transition, command)| PublicV1TopologyPlan::Begin {
            transition: Box::new(transition),
            command: Box::new(command),
        },
    )
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
    prepare_retained: bool,
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
    if role_before_epoch(local.report.role, prebuilt, prepare_retained) {
        Ok(vec![role, epoch, configuration])
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

fn role_before_epoch(role: ReplicaRole, prebuilt: bool, prepare_retained: bool) -> bool {
    prepare_retained && role == ReplicaRole::None && !prebuilt
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
        Some(PublicOperationProgram::Swap {
            mode,
            catch_up_specific_quorum,
            ..
        }) => (Some(*mode), Some(*catch_up_specific_quorum)),
        _ => (None, None),
    };
    let transition = PublicV1AcceptedTransitionStatus {
        resource_version: resource_version.to_string(),
        evidence: accepted.clone(),
        intent: PublicV1AcceptedTransitionIntent {
            consumer: if call_kind == PublicV1CallKind::Swap {
                PublicV1EvidenceConsumer::Switchover
            } else {
                PublicV1EvidenceConsumer::Topology
            },
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
        Some(PublicOperationProgram::Swap { refreshed, .. }) => {
            (None, Some(refreshed.current.configuration_id.clone()))
        }
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
        let mut replica = PublicV1TopologyReplica {
            report: PublicV1AgentReport {
                identity: PublicV1Identity::new(1),
                resource_uid: resource_uid.clone(),
                replica: identity.clone(),
                process_session_id: ProcessSessionId::new(format!("session-{id}")),
                replicator_creation_id: ReplicatorCreationId::new(format!("creation-{id}")),
                report_sequence: 1,
                open_mode: kuberic_runtime::application::OpenMode::Existing,
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
        };
        let program = PublicOperationProgram::Open {
            replica: replica.report.replica.clone(),
            existing: true,
        };
        replica
            .report
            .completed_operations
            .push(PublicV1OperationCompletion {
                operation_id: OperationId::new(format!("observed-open-{id}")),
                revision: 1,
                call_kind: PublicV1CallKind::Open,
                process_session_id: replica.report.process_session_id.clone(),
                input_digest: program.digest(),
                replay_disposition: program.replay_disposition(),
                history: replica.report.history.clone(),
                storage_incarnation: replica.report.storage_incarnation.clone(),
                previous_configuration_id: None,
                current_configuration_id: None,
                catch_up_mode: None,
                catch_up_specific_quorum: None,
                result: PublicV1OperationResult::Succeeded,
            });
        replica
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
            PublicOperationProgram::Open { .. } => {
                report.report.open_mode = kuberic_runtime::application::OpenMode::Existing;
                (PublicV1CallKind::Open, None, None, None, None)
            }
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
        if call_kind == PublicV1CallKind::Open {
            report.report.completed_operations.push(completion.clone());
        }
        accepted
            .completed_operations
            .push(PublicV1AcceptedOperationCompletion::from_completion(
                &completion,
            ));
    }

    #[test]
    fn open_endpoint_requires_exact_current_successful_open_evidence() {
        let original = replica(1, ReplicaRole::Primary).report;
        assert!(original.has_current_open_endpoint());
        let mut initialized = original.clone();
        initialized.completed_operations[0].input_digest = PublicOperationProgram::Open {
            replica: original.replica.clone(),
            existing: false,
        }
        .digest();
        assert!(initialized.has_current_open_endpoint());
        for mutation in 0..7 {
            let mut report = original.clone();
            match mutation {
                0 => report.process_session_id = ProcessSessionId::new("restarted-session"),
                1 => {
                    report.storage_incarnation.storage_generation =
                        StorageGeneration::new("replaced-storage")
                }
                2 => report.completed_operations[0].history.data_loss_number += 1,
                3 => report.completed_operations[0].call_kind = PublicV1CallKind::Epoch,
                4 => {
                    report.completed_operations[0].input_digest = PublicOperationProgram::Epoch {
                        epoch: Epoch::new(0, 1),
                    }
                    .digest()
                }
                5 => {
                    report.completed_operations[0].result =
                        PublicV1OperationResult::Failed("failed Open".into())
                }
                6 => report.replication_address = None,
                _ => unreachable!(),
            }
            assert!(!report.has_current_open_endpoint(), "mutation {mutation}");
        }
    }

    #[test]
    fn ordinary_failover_advances_epoch_before_assigning_a_fenced_role() {
        assert!(!role_before_epoch(ReplicaRole::None, false, false));
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
            replica.report.completed_operations.clear();
        }
        replicas[0].report.open_mode = kuberic_runtime::application::OpenMode::New;
        replicas[2].report.open_mode = kuberic_runtime::application::OpenMode::New;
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
            assert_eq!(
                command.intent.program,
                Some(PublicOperationProgram::Open {
                    replica: replicas[index].report.replica.clone(),
                    existing: replicas[index].report.open_mode
                        == kuberic_runtime::application::OpenMode::Existing,
                })
            );
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
            if index == 0 {
                for mutation in 0..3 {
                    let mut stale = accepted.clone();
                    let completion = stale.completed_operations.last_mut().unwrap();
                    match mutation {
                        0 => completion.process_session_id = ProcessSessionId::new("old-session"),
                        1 => {
                            completion.storage_incarnation.storage_generation =
                                StorageGeneration::new("old-storage")
                        }
                        2 => completion.input_digest_sha256 = "0".repeat(64),
                        _ => unreachable!(),
                    }
                    assert!(matches!(
                        plan_public_v1_topology(
                            &stale,
                            None,
                            &replicas,
                            3,
                            None,
                            None,
                            "resource-version",
                        )
                        .unwrap(),
                        PublicV1TopologyPlan::Begin { .. }
                    ));
                }
            }
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
                        "topology preparation must not reopen a process with a successful Open endpoint"
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
    fn replacement_open_uses_host_mode_and_is_not_repeated_after_endpoint_publication() {
        use kuberic_runtime::application::OpenMode;

        for mode in [OpenMode::New, OpenMode::Existing] {
            let mut replicas = vec![
                replica(1, ReplicaRole::Primary),
                replica(2, ReplicaRole::ActiveSecondary),
                replica(3, ReplicaRole::ActiveSecondary),
            ];
            let current = desired_configuration(None, &replicas, ReplicaId::new(1)).unwrap();
            let mut accepted = accepted(&replicas[0]);
            replicas[2].report.replica.instance_id = ReplicaInstanceId::new("replacement-pod");
            replicas[2].report.storage_incarnation.replica = replicas[2].report.replica.clone();
            replicas[2].report.open_mode = mode;
            replicas[2].report.role = ReplicaRole::None;
            replicas[2].report.progress.role = PublicProgressRole::Recovery;
            replicas[2].report.replication_address = None;
            replicas[2].report.completed_operations.clear();
            replicas[2].replication_address.clear();
            let PublicV1TopologyPlan::Begin { command, .. } = plan_public_v1_topology(
                &accepted,
                Some(&current),
                &replicas,
                3,
                None,
                None,
                "resource-version",
            )
            .unwrap() else {
                panic!("replacement must authorize its host-selected Open");
            };
            assert_eq!(
                command.intent.program,
                Some(PublicOperationProgram::Open {
                    replica: replicas[2].report.replica.clone(),
                    existing: mode == OpenMode::Existing,
                })
            );
            assert_eq!(
                command.intent.input_digest,
                command.intent.program.as_ref().unwrap().digest()
            );
            accept_command(&mut accepted, &mut replicas, &command);
            let address = "http://127.0.0.1:41003".to_string();
            replicas[2].report.replication_address = Some(address.clone());
            replicas[2].replication_address = address;
            let PublicV1TopologyPlan::Begin { command, .. } = plan_public_v1_topology(
                &accepted,
                Some(&current),
                &replicas,
                3,
                None,
                None,
                "resource-version",
            )
            .unwrap() else {
                panic!("opened replacement must proceed to Build preparation");
            };
            assert!(!matches!(
                command.intent.program,
                Some(PublicOperationProgram::Open { .. })
            ));
        }
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
    fn clean_rejoin_requires_exact_accepted_storage_session_and_delta_catch_up() {
        let mut replicas = vec![
            replica(1, ReplicaRole::Primary),
            replica(2, ReplicaRole::ActiveSecondary),
            replica(3, ReplicaRole::ActiveSecondary),
        ];
        let mut accepted = accepted(&replicas[0]);
        let original = desired_configuration(None, &replicas, ReplicaId::new(1)).unwrap();
        let program = PublicOperationProgram::Configuration(
            public_configuration(&replicas[0].report.replica, &replicas, &original).unwrap(),
        );
        let (_, command) = begin_operation(
            &accepted,
            &replicas[0],
            program,
            OperationId::new("historical-configuration"),
            "resource-version",
        )
        .unwrap();
        accept_command(&mut accepted, &mut replicas, &command);
        let storage_incarnation = replicas[0].report.storage_incarnation.clone();
        replicas[0]
            .report
            .completed_operations
            .push(PublicV1OperationCompletion {
                operation_id: command.intent.operation_id.clone(),
                revision: command.intent.revision,
                call_kind: PublicV1CallKind::Configuration,
                process_session_id: command.intent.process_session_id.clone(),
                input_digest: command.intent.input_digest.clone(),
                replay_disposition: command.intent.replay_disposition,
                history: accepted.history.clone(),
                storage_incarnation,
                previous_configuration_id: None,
                current_configuration_id: Some(original.configuration_id.clone()),
                catch_up_mode: None,
                catch_up_specific_quorum: None,
                result: PublicV1OperationResult::Succeeded,
            });
        replicas[0].report.progress.first = 1;
        replicas[0].report.progress.last = 1;
        replicas[1] = replica(2, ReplicaRole::Primary);
        replicas[1].report.progress.first = 2;
        replicas[1].report.progress.last = 2;
        let current =
            desired_configuration(Some(&original), &replicas[1..], ReplicaId::new(2)).unwrap();
        assert!(can_rejoin_retained_member(
            &accepted,
            &current,
            &replicas,
            &replicas[0]
        ));
        for mutation in 0..8 {
            let mut target = replicas[0].clone();
            let mut evidence = accepted.clone();
            match mutation {
                0 => {
                    target.report.storage_incarnation.storage_generation =
                        StorageGeneration::new("fresh-storage")
                }
                1 => target.report.process_session_id = ProcessSessionId::new("fresh-session"),
                2 => {
                    target.report.history.history_id =
                        kuberic_runtime::protocol::types::HistoryId::new("other-history")
                }
                3 => target.report.progress.last = 2,
                4 => target.report.completed_operations.clear(),
                5 => evidence.completed_operations.clear(),
                6 => {
                    target.report.progress.first = 3;
                    target.report.progress.last = 3;
                }
                7 => {
                    let mut built = evidence.completed_operations[0].clone();
                    built.operation_id = topology_build_id(&current, &replicas[1], &target);
                    built.call_kind = PublicV1CallKind::Build;
                    evidence.completed_operations.push(built);
                }
                _ => unreachable!(),
            }
            assert!(
                !can_rejoin_retained_member(&evidence, &current, &replicas, &target),
                "unsafe retained rejoin mutation {mutation}"
            );
        }
        let mut prepared = Vec::new();
        for _ in 0..20 {
            let plan = plan_public_v1_topology(
                &accepted,
                Some(&current),
                &replicas,
                3,
                None,
                Some(ReplicaId::new(2)),
                "resource-version",
            )
            .unwrap();
            let PublicV1TopologyPlan::Begin { command, .. } = plan else {
                panic!("rejoin must catch up before persisting topology: {plan:?}");
            };
            match command.intent.program.as_ref().unwrap() {
                PublicOperationProgram::Build(_) => panic!("clean rejoin must not snapshot-copy"),
                PublicOperationProgram::Configuration(_) => prepared.push(command.target.clone()),
                PublicOperationProgram::CatchUp { mode, .. } => {
                    assert_eq!(*mode, PublicCatchUpMode::All);
                    assert_eq!(command.target.replica_id, ReplicaId::new(2));
                    assert!(prepared.contains(&replicas[0].report.replica));
                    assert!(prepared.contains(&replicas[2].report.replica));
                    return;
                }
                _ => {}
            }
            accept_command(&mut accepted, &mut replicas, &command);
        }
        panic!("clean rejoin never reached delta catch-up");
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
