use kuberic_runtime::protocol::command::PublicV1OperationCommand;
use kuberic_runtime::protocol::public_operations::{
    PublicBuildInput, PublicCatchUpMode, PublicConfiguration, PublicOperationClass,
    PublicOperationIntent, PublicOperationProgram, PublicReplicaDescriptor, PublicV1AcceptedResult,
    PublicV1AcceptedStatus, PublicV1AgentReport, PublicV1CallKind, PublicV1OperationResult,
};
use kuberic_runtime::protocol::types::{
    ConfigurationDescriptor, ConfigurationId, ConfigurationMember, Epoch, OpenMode, OperationId,
    ReplicaId, ReplicaRole,
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
            replica.report.validate().is_ok()
                && replica.report.identity == accepted.identity
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

    let unopened = eligible
        .iter()
        .any(|replica| !replica.report.has_current_open_endpoint());
    let primary_id = if unopened {
        // A deterministic provisional ID only addresses Open work. No role,
        // configuration or election is admitted until native queries exist.
        current
            .map(|configuration| configuration.primary_id)
            .filter(|primary| {
                eligible
                    .iter()
                    .any(|replica| replica.report.replica.replica_id == *primary)
            })
            .or_else(|| {
                eligible
                    .iter()
                    .map(|replica| replica.report.replica.replica_id)
                    .min()
            })
            .ok_or("no replica can be opened")?
    } else {
        if current.is_some_and(|configuration| {
            eligible.iter().any(|replica| {
                replica.report.replica.replica_id == configuration.primary_id
                    && replica.report.progress.current().is_none()
            })
        }) || eligible
            .iter()
            .all(|replica| replica.report.progress.current().is_none())
        {
            return Ok(PublicV1TopologyPlan::Wait);
        }
        choose_primary(current, &eligible, switchover_target)?
    };
    let bootstrap = current.is_none().then(|| {
        eligible
            .iter()
            .filter(|replica| replica.report.replica.replica_id == primary_id)
            .cloned()
            .collect::<Vec<_>>()
    });
    let mut next = desired_configuration(
        current,
        bootstrap.as_deref().unwrap_or(&eligible),
        primary_id,
        accepted.history.data_loss_number,
    )?;
    if let Some(known) = eligible
        .iter()
        .find(|replica| replica.report.replica.replica_id == primary_id)
        .and_then(|replica| replica.report.configuration.as_ref())
        .filter(|observation| observation.voting.current.primary_id == primary_id)
        .map(|observation| &observation.voting)
    {
        let same_members = known.current.primary_id == next.primary_id
            && known.current.members == next.members
            && known.current.write_quorum == next.write_quorum
            && known
                .replicas
                .iter()
                .filter(|peer| {
                    known
                        .current
                        .members
                        .iter()
                        .any(|member| member.identity == peer.identity)
                })
                .all(|peer| {
                    eligible.iter().any(|replica| {
                        replica.report.replica == peer.identity
                            && replica.report.process_session_id == peer.process_session_id
                            && Some(&replica.report.storage_incarnation)
                                == peer.storage_incarnation.as_ref()
                            && replica.replication_address == peer.replication_address
                    })
                });
        let number = if same_members {
            next.epoch
                .configuration_number
                .max(known.current.epoch.configuration_number)
        } else {
            next.epoch.configuration_number.max(
                known
                    .current
                    .epoch
                    .configuration_number
                    .checked_add(1)
                    .ok_or("native configuration epoch exhausted")?,
            )
        };
        next = ConfigurationDescriptor::new(
            Epoch::new(accepted.history.data_loss_number, number),
            next.primary_id,
            next.members,
            next.write_quorum,
        );
    }
    if current == Some(&next) && configuration_is_admitted(accepted, &next, &eligible) {
        if let Some(plan) = native_configuration_plan(accepted, &eligible, &next, resource_version)?
        {
            return Ok(plan);
        }
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
            existing: replica.report.open_mode == OpenMode::Existing,
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
                    if step == "role" && peer.report.role == ReplicaRole::ActiveSecondary {
                        continue;
                    }
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
                && !eligible.iter().any(|source| {
                    current.members.iter().any(|member| {
                        member.role == ReplicaRole::Primary
                            && member.identity == source.report.replica
                    }) && accepted.completed_operations.iter().any(|completion| {
                        completion.operation_id == topology_build_id(current, source, replica)
                            && completion.result == PublicV1AcceptedResult::Succeeded
                    })
                })
        })
    {
        let role = (
            "build-role",
            PublicOperationProgram::Role {
                epoch: current.epoch,
                role: ReplicaRole::IdleSecondary,
            },
        );
        let preparation = [role];
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
            if let Some((identity, program)) =
                native_cold_copy_step(&eligible, source, target, operational_next.epoch)?
            {
                let replica = eligible
                    .iter()
                    .find(|replica| replica.report.replica == identity)
                    .ok_or("native copy preparation lost its exact participant")?;
                return begin_plan(
                    accepted,
                    replica,
                    program,
                    &format!(
                        "topology-{}-prepare-copy-{}-{}-{}-{}-",
                        current.configuration_id,
                        target.report.replica.replica_id,
                        target.report.process_session_id,
                        replica.report.replica.replica_id,
                        replica.report.process_session_id
                    ),
                    resource_version,
                );
            }
            if source.report.role != ReplicaRole::Primary
                || source.report.read_status
                    != kuberic_runtime::protocol::types::AccessStatus::Granted
            {
                return Ok(PublicV1TopologyPlan::Wait);
            }
            let build_base = source
                .report
                .configuration
                .as_ref()
                .map(|observation| &observation.voting.current)
                .unwrap_or(current);
            return begin_build(
                accepted,
                build_base,
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
    if operational_replicas.iter().any(|replica| {
        replica.report.replica.replica_id == operational_next.primary_id
            && replica.report.role == ReplicaRole::Primary
            && replica.report.configuration.is_some()
    }) && let Some(plan) = native_configuration_plan(
        accepted,
        operational_replicas,
        operational_next,
        resource_version,
    )? {
        return Ok(plan);
    }
    let mut ordered_replicas = operational_replicas
        .iter()
        .filter(|replica| {
            operational_next
                .members
                .iter()
                .any(|member| member.identity == replica.report.replica)
        })
        .collect::<Vec<_>>();
    let changing_primary = current
        .is_some_and(|configuration| configuration.primary_id != operational_next.primary_id);
    if changing_primary || !retained_rejoins.is_empty() {
        ordered_replicas.sort_by_key(|replica| {
            replica.report.replica.replica_id == operational_next.primary_id
        });
    }
    for replica in ordered_replicas {
        if replica.report.configuration.as_ref().is_some_and(|observation| {
            let configuration = &observation.voting;
            configuration.current == *operational_next && configuration.previous.is_none()
        }) && replica.report.completed_operations.iter().any(|completion| {
            completion.current_configuration_id.as_ref()
                == Some(&operational_next.configuration_id)
                && completion.process_session_id == replica.report.process_session_id
                && completion.storage_incarnation == replica.report.storage_incarnation
                && completion.result == PublicV1OperationResult::Succeeded
                && accepted.completed_operations.contains(
                    &kuberic_runtime::protocol::public_operations::PublicV1AcceptedOperationCompletion::from_completion(completion),
                )
        }) {
            continue;
        }
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
            source_address: source.replication_address.clone(),
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
        && report
            .progress
            .current()
            .is_some_and(|progress| progress >= 0)
        && replicas.iter().any(|source| {
            current.members.iter().any(|member| {
                member.role == ReplicaRole::Primary && member.identity == source.report.replica
            }) && source
                .report
                .progress
                .current()
                .zip(report.progress.current())
                .is_some_and(|(source_progress, target_progress)| {
                    source_progress >= target_progress
                })
                && source
                    .report
                    .progress
                    .capability()
                    .zip(report.progress.current())
                    .is_some_and(|(retained, target_progress)| {
                        source.report.progress.current() == Some(target_progress)
                            || (retained > 0 && retained <= target_progress.saturating_add(1))
                    })
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
        source_address: source.replication_address.clone(),
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

pub(crate) fn content_addressed_operation_id(
    prefix: String,
    program: &PublicOperationProgram,
) -> OperationId {
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
    let admitted_member = |replica: &&PublicV1TopologyReplica| {
        current.is_none_or(|configuration| {
            configuration
                .members
                .iter()
                .any(|member| member.identity == replica.report.replica)
        })
    };
    if let Some(target) = switchover_target {
        if replicas.iter().any(|replica| {
            replica.report.replica.replica_id == target
                && admitted_member(&replica)
                && replica
                    .report
                    .progress
                    .current()
                    .is_some_and(|progress| progress >= 0)
        }) {
            return Ok(target);
        }
        return Err("planned switchover target is unavailable".into());
    }
    if current.is_none() && !replicas.is_empty() && replicas.iter().all(fresh_unknown_history) {
        // Local initial authority, not a retained-history election or a made-up
        // progress value. The native database can still be uninitialized here.
        return replicas
            .iter()
            .map(|replica| replica.report.replica.replica_id)
            .min()
            .ok_or_else(|| "no fresh initial replica".into());
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
        .filter(admitted_member)
        .filter(|replica| {
            replica.report.role == ReplicaRole::Primary
                && replica
                    .report
                    .progress
                    .current()
                    .is_some_and(|progress| progress >= 0)
        })
        .map(|replica| replica.report.replica.replica_id)
        .collect::<Vec<_>>();
    match promoted.as_slice() {
        [primary] => return Ok(*primary),
        [] => {}
        _ => return Err("public-v1 topology observed multiple promoted primaries".into()),
    }
    replicas
        .iter()
        .filter(admitted_member)
        .filter(|replica| {
            replica
                .report
                .progress
                .current()
                .is_some_and(|progress| progress >= 0)
        })
        .max_by(|left, right| {
            left.report
                .history
                .compare_progress(
                    left.report
                        .progress
                        .current()
                        .expect("filtered successful native query"),
                    &right.report.history,
                    right
                        .report
                        .progress
                        .current()
                        .expect("filtered successful native query"),
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
    data_loss_number: i64,
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
        Epoch::new(data_loss_number, 1)
    };
    let write_quorum = majority(members.len());
    Ok(ConfigurationDescriptor::new(
        epoch,
        primary_id,
        members,
        write_quorum,
    ))
}

fn fresh_unknown_history(replica: &PublicV1TopologyReplica) -> bool {
    let report = &replica.report;
    let fresh_open = PublicOperationProgram::Open {
        replica: report.replica.clone(),
        existing: false,
    };
    report.has_current_open_endpoint()
        && report.role == ReplicaRole::None
        && report.configuration.is_none()
        && report.engine_idle.is_none()
        && report.remote_builds.is_empty()
        && report.retry_pending_operations.is_empty()
        && report.data_loss_recovery.is_none()
        && report.active_operations.is_empty()
        && report.progress.current()
            == Some(kuberic_runtime::protocol::public_operations::INVALID_LSN)
        && report.progress.capability()
            == Some(kuberic_runtime::protocol::public_operations::INVALID_LSN)
        && !report.completed_operations.is_empty()
        && report.completed_operations.iter().all(|completion| {
            completion.call_kind == PublicV1CallKind::Open
                && completion.result == PublicV1OperationResult::Succeeded
                && completion.input_digest == fresh_open.digest()
                && completion.process_session_id == report.process_session_id
                && completion.storage_incarnation == report.storage_incarnation
                && completion.history == report.history
        })
}

fn majority(count: usize) -> u32 {
    (count as u32) / 2 + 1
}

fn configuration_is_admitted(
    accepted: &PublicV1AcceptedStatus,
    configuration: &ConfigurationDescriptor,
    replicas: &[PublicV1TopologyReplica],
) -> bool {
    configuration.members.iter().all(|member| {
        replicas.iter().any(|replica| {
            replica.report.replica == member.identity
                && replica.report.validate().is_ok()
                && replica.report.progress.current().is_some()
                && !replica.replication_address.is_empty()
                && replica.report.role == member.role
                && match member.role {
                    ReplicaRole::Primary => {
                        replica.report.write_status
                            == kuberic_runtime::protocol::types::AccessStatus::Granted
                            || (configuration.members.len() == 1
                                && replica.report.read_status
                                    == kuberic_runtime::protocol::types::AccessStatus::Granted
                                && replica.report.write_status
                                    == kuberic_runtime::protocol::types::AccessStatus::NoWriteQuorum)
                    }
                    ReplicaRole::ActiveSecondary => replica.report.write_status
                        == kuberic_runtime::protocol::types::AccessStatus::NotPrimary
                        && replica.report.read_status
                            == kuberic_runtime::protocol::types::AccessStatus::NotPrimary,
                    _ => false,
                }
                && replica.report.completed_operations.iter().any(|completion| {
                    completion.process_session_id == replica.report.process_session_id
                        && completion.storage_incarnation == replica.report.storage_incarnation
                        && completion.history == accepted.history
                        && completion.current_configuration_id.as_ref() == Some(&configuration.configuration_id)
                        && matches!(completion.call_kind,
                            PublicV1CallKind::Configuration | PublicV1CallKind::CatchUp
                            | PublicV1CallKind::InitialPrimary | PublicV1CallKind::FailoverPromotion
                            | PublicV1CallKind::SecondaryEpochAdvance)
                        && completion.result == PublicV1OperationResult::Succeeded
                        && accepted.completed_operations.contains(
                            &kuberic_runtime::protocol::public_operations::PublicV1AcceptedOperationCompletion::from_completion(completion)
                        )
                })
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
    let unchanged_role = local.report.role == role;
    let promoting_secondary =
        role == ReplicaRole::Primary && local.report.role != ReplicaRole::None && !unchanged_role;
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
    let configuration = if catch_up_primary || promoting_secondary {
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
    if unchanged_role {
        Ok(vec![epoch, configuration])
    } else if role_before_epoch(local.report.role, prebuilt, prepare_retained) {
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
                current_progress: replica
                    .report
                    .progress
                    .current()
                    .unwrap_or(kuberic_runtime::protocol::public_operations::INVALID_LSN),
                catch_up_capability: replica
                    .report
                    .progress
                    .capability()
                    .unwrap_or(kuberic_runtime::protocol::public_operations::INVALID_LSN),
                must_catchup: false,
                is_up: replica.report.has_current_open_endpoint(),
                is_ready: replica.report.has_current_open_endpoint()
                    && replica.report.progress.current().is_some()
                    && replica.report.progress.capability().is_some(),
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

fn completed_program(replica: &PublicV1TopologyReplica, program: &PublicOperationProgram) -> bool {
    let digest = program.digest();
    replica
        .report
        .completed_operations
        .iter()
        .any(|completion| {
            completion.process_session_id == replica.report.process_session_id
                && completion.storage_incarnation == replica.report.storage_incarnation
                && completion.history == replica.report.history
                && completion.input_digest == digest
                && completion.result == PublicV1OperationResult::Succeeded
        })
}

fn same_bindings(left: &PublicConfiguration, right: &PublicConfiguration) -> bool {
    left.current == right.current
        && left.previous == right.previous
        && left.replicas.len() == right.replicas.len()
        && left.replicas.iter().all(|peer| {
            right.replicas.iter().any(|other| {
                peer.identity == other.identity
                    && peer.process_session_id == other.process_session_id
                    && peer.storage_incarnation == other.storage_incarnation
                    && peer.replication_address == other.replication_address
                    && peer.role == other.role
                    && peer.is_up == other.is_up
                    && peer.is_ready == other.is_ready
            })
        })
}

pub(crate) fn native_public_configuration(
    local: &kuberic_runtime::protocol::types::ReplicaIdentity,
    replicas: &[PublicV1TopologyReplica],
    current: &ConfigurationDescriptor,
    previous: Option<&ConfigurationDescriptor>,
    known: Option<&PublicConfiguration>,
) -> Result<PublicConfiguration, String> {
    let mut peers = Vec::new();
    for member in current.members.iter().chain(
        previous
            .into_iter()
            .flat_map(|configuration| &configuration.members),
    ) {
        if member.identity == *local
            || peers
                .iter()
                .any(|peer: &PublicReplicaDescriptor| peer.identity == member.identity)
        {
            continue;
        }
        let in_current = current
            .members
            .iter()
            .any(|current| current.identity == member.identity);
        let historical = known.and_then(|configuration| {
            configuration
                .replicas
                .iter()
                .find(|peer| peer.identity == member.identity)
        });
        let observed = replicas
            .iter()
            .find(|replica| replica.report.replica == member.identity);
        let mut peer = if let Some(historical) = historical.filter(|_| !in_current) {
            let mut peer = historical.clone();
            peer.current_progress = kuberic_runtime::protocol::public_operations::INVALID_LSN;
            peer.catch_up_capability = kuberic_runtime::protocol::public_operations::INVALID_LSN;
            let exact = observed.filter(|observed| {
                observed.report.process_session_id == peer.process_session_id
                    && Some(&observed.report.storage_incarnation)
                        == peer.storage_incarnation.as_ref()
                    && observed.replication_address == peer.replication_address
            });
            peer.is_up = exact.is_some_and(|observed| observed.report.has_current_open_endpoint());
            peer.is_ready = exact.is_some_and(|observed| {
                observed.report.has_current_open_endpoint()
                    && observed.report.progress.current().is_some()
                    && observed.report.progress.capability().is_some()
            });
            peer
        } else {
            let observed = observed.ok_or("configuration lacks exact peer authority")?;
            PublicReplicaDescriptor {
                build_id: OperationId::default(),
                identity: observed.report.replica.clone(),
                replication_address: observed.replication_address.clone(),
                process_session_id: observed.report.process_session_id.clone(),
                storage_incarnation: Some(observed.report.storage_incarnation.clone()),
                role: member.role,
                current_progress: observed
                    .report
                    .progress
                    .current()
                    .unwrap_or(kuberic_runtime::protocol::public_operations::INVALID_LSN),
                catch_up_capability: observed
                    .report
                    .progress
                    .capability()
                    .unwrap_or(kuberic_runtime::protocol::public_operations::INVALID_LSN),
                must_catchup: false,
                is_up: observed.report.has_current_open_endpoint(),
                is_ready: observed.report.has_current_open_endpoint()
                    && observed.report.progress.current().is_some()
                    && observed.report.progress.capability().is_some(),
            }
        };
        peer.role = member.role;
        peers.push(peer);
    }
    let input = PublicConfiguration {
        current: current.clone(),
        previous: previous.cloned(),
        replicas: peers,
    };
    input.validate().map_err(str::to_owned)?;
    if input.replicas.iter().any(|peer| peer.identity == *local)
        || !input
            .current
            .members
            .iter()
            .chain(input.previous.iter().flat_map(|previous| &previous.members))
            .any(|member| member.identity == *local)
    {
        return Err("configuration does not describe its exact local participant".into());
    }
    Ok(input)
}

fn native_configuration_plan(
    accepted: &PublicV1AcceptedStatus,
    replicas: &[PublicV1TopologyReplica],
    desired: &ConfigurationDescriptor,
    resource_version: &str,
) -> Result<Option<PublicV1TopologyPlan>, String> {
    let Some((identity, program)) = native_configuration_step(replicas, desired)? else {
        return Ok(None);
    };
    let replica = replicas
        .iter()
        .find(|replica| replica.report.replica == identity)
        .ok_or("native configuration lost its exact participant")?;
    begin_plan(
        accepted,
        replica,
        program,
        &format!(
            "topology-{}-native-{}-{}-",
            desired.configuration_id,
            replica.report.replica.replica_id,
            replica.report.process_session_id
        ),
        resource_version,
    )
    .map(Some)
}

pub(crate) fn native_configuration_step(
    replicas: &[PublicV1TopologyReplica],
    desired: &ConfigurationDescriptor,
) -> Result<
    Option<(
        kuberic_runtime::protocol::types::ReplicaIdentity,
        PublicOperationProgram,
    )>,
    String,
> {
    let source = replicas
        .iter()
        .find(|replica| replica.report.replica.replica_id == desired.primary_id)
        .ok_or("configuration has no exact primary observation")?;
    let known = source
        .report
        .configuration
        .as_ref()
        .map(|observation| &observation.voting);
    let installed =
        native_public_configuration(&source.report.replica, replicas, desired, None, known)?;
    // Renewal may have finalized a retained voter set at this epoch before
    // copying the replacement. Adding its now-built peer is a fresh current-set
    // synchronization (native UpdateReplicas analogue), not reopening PC/CC.
    let synchronize_current = known.is_some_and(|known| {
        known.previous.is_none()
            && known.current.epoch == desired.epoch
            && !same_bindings(known, &installed)
    });
    let previous = known
        .filter(|known| {
            !synchronize_current && (known.current != *desired || !same_bindings(known, &installed))
        })
        .map(|known| &known.current);
    let previous = match known {
        Some(known) if known.current == *desired && known.previous.is_some() => {
            known.previous.as_ref()
        }
        _ => previous,
    };
    for member in &desired.members {
        if member.identity == source.report.replica {
            continue;
        }
        let replica = replicas
            .iter()
            .find(|replica| replica.report.replica == member.identity)
            .ok_or("configuration member has no exact report")?;
        let role = PublicOperationProgram::Role {
            epoch: desired.epoch,
            role: member.role,
        };
        if replica.report.role != member.role {
            return Ok(Some((member.identity.clone(), role)));
        }
        let epoch = PublicOperationProgram::Epoch {
            epoch: desired.epoch,
        };
        if !completed_program(replica, &epoch) {
            return Ok(Some((member.identity.clone(), epoch)));
        }
        let input =
            native_public_configuration(&member.identity, replicas, desired, previous, known)?;
        if !replica
            .report
            .configuration
            .as_ref()
            .is_some_and(|known| same_bindings(&known.voting, &input))
        {
            return Ok(Some((
                member.identity.clone(),
                PublicOperationProgram::Configuration(input),
            )));
        }
    }
    let epoch = PublicOperationProgram::Epoch {
        epoch: desired.epoch,
    };
    if !completed_program(source, &epoch) {
        return Ok(Some((source.report.replica.clone(), epoch)));
    }
    if let Some(previous) = previous
        && !known.is_some_and(|known| {
            known.current == *desired && known.previous.as_ref() == Some(previous)
        })
    {
        return Ok(Some((
            source.report.replica.clone(),
            PublicOperationProgram::CatchUp {
                configuration: native_public_configuration(
                    &source.report.replica,
                    replicas,
                    desired,
                    Some(previous),
                    known,
                )?,
                mode: PublicCatchUpMode::WriteQuorum,
            },
        )));
    }
    if !known.is_some_and(|known| same_bindings(known, &installed)) {
        return Ok(Some((
            source.report.replica.clone(),
            if synchronize_current {
                PublicOperationProgram::CatchUp {
                    configuration: installed,
                    mode: PublicCatchUpMode::WriteQuorum,
                }
            } else {
                PublicOperationProgram::Configuration(installed)
            },
        )));
    }
    Ok(None)
}

pub(crate) fn native_cold_copy_step(
    replicas: &[PublicV1TopologyReplica],
    source: &PublicV1TopologyReplica,
    target: &PublicV1TopologyReplica,
    epoch: Epoch,
) -> Result<
    Option<(
        kuberic_runtime::protocol::types::ReplicaIdentity,
        PublicOperationProgram,
    )>,
    String,
> {
    let Some(observation) = source.report.configuration.as_ref() else {
        return Ok(None);
    };
    let installed = &observation.voting;
    if installed
        .current
        .members
        .iter()
        .any(|member| member.identity.replica_id == target.report.replica.replica_id)
    {
        let members = installed
            .current
            .members
            .iter()
            .filter(|member| {
                member.identity == source.report.replica
                    || (member.identity.replica_id != target.report.replica.replica_id
                        && replicas.iter().any(|replica| {
                            replica.report.replica == member.identity
                                && replica.report.progress.current().is_some()
                                && installed.replicas.iter().any(|known| {
                                    known.identity == replica.report.replica
                                        && known.process_session_id
                                            == replica.report.process_session_id
                                        && known.storage_incarnation.as_ref()
                                            == Some(&replica.report.storage_incarnation)
                                        && known.replication_address == replica.replication_address
                                })
                        }))
            })
            .cloned()
            .map(|mut member| {
                member.role = if member.identity == source.report.replica {
                    ReplicaRole::Primary
                } else {
                    ReplicaRole::ActiveSecondary
                };
                member
            })
            .collect::<Vec<_>>();
        let number = installed
            .current
            .epoch
            .configuration_number
            .checked_add(1)
            .ok_or("native configuration epoch exhausted")?
            .max(epoch.configuration_number);
        let retained = ConfigurationDescriptor::new(
            Epoch::new(epoch.data_loss_number, number),
            source.report.replica.replica_id,
            members.clone(),
            installed.current.write_quorum.min(members.len() as u32),
        );
        return native_configuration_step(replicas, &retained);
    }
    if installed.previous.is_some() {
        return native_configuration_step(replicas, &installed.current);
    }
    // Exact retained ownership requires exact retirement even without a diagnostic.
    // An optional native idle query may only infer an otherwise unowned idle slot.
    if let Some(remote) = source.report.remote_builds.iter().find(|remote| {
        remote.build.replica.replica_id == target.report.replica.replica_id
            && matches!(
                remote.state,
                kuberic_runtime::protocol::public_operations::PublicV1RemoteBuildState::Accepted
                    | kuberic_runtime::protocol::public_operations::PublicV1RemoteBuildState::InBuild
                    | kuberic_runtime::protocol::public_operations::PublicV1RemoteBuildState::Ready
                    | kuberic_runtime::protocol::public_operations::PublicV1RemoteBuildState::InDrop
            )
    }) {
        return Ok(Some((
            source.report.replica.clone(),
            PublicOperationProgram::Remove(remote.build.clone()),
        )));
    }
    if source.report.engine_idle.as_ref().is_some_and(|sample| {
        sample
            .idle_replicas
            .contains(&target.report.replica.replica_id)
    }) {
        return Ok(Some((
            source.report.replica.clone(),
            PublicOperationProgram::RetireIdleReplica {
                replica_id: target.report.replica.replica_id,
            },
        )));
    }
    Ok(None)
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
    if matches!(program, PublicOperationProgram::Remove(_))
        && let Some(active) = replica
            .report
            .active_operations
            .iter()
            .find(|active| active.operation_id == *operation_id)
    {
        if active.identity != accepted.identity
            || replica.report.history != accepted.history
            || active.process_session_id != replica.report.process_session_id
            || active.input_digest != digest
            || active.program.as_ref() != Some(program)
        {
            return Err("active operation differs from exact topology input".into());
        }
        return Ok(active.revision);
    }
    if let Some(pending) = replica
        .report
        .retry_pending_operations
        .iter()
        .find(|pending| pending.intent.operation_id == *operation_id)
    {
        if pending.intent.identity != accepted.identity
            || replica.report.history != accepted.history
            || pending.intent.process_session_id != replica.report.process_session_id
            || pending.intent.input_digest != digest
            || pending.intent.program.as_ref() != Some(program)
        {
            return Err("retry-pending operation differs from exact topology input".into());
        }
        return Ok(pending.intent.revision);
    }
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
    if let PublicOperationProgram::Remove(build) = program {
        return exact_remove_revision(accepted, replica, operation_id, build);
    }
    replica
        .report
        .completed_operations
        .iter()
        .map(|completion| completion.revision)
        .chain(
            replica
                .report
                .retry_pending_operations
                .iter()
                .map(|pending| pending.intent.revision),
        )
        .chain(
            replica
                .report
                .active_operations
                .iter()
                .map(|intent| intent.revision),
        )
        .chain(
            replica
                .report
                .remote_builds
                .iter()
                .map(|build| build.revision),
        )
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

pub(crate) fn exact_remove_revision(
    accepted: &PublicV1AcceptedStatus,
    replica: &PublicV1TopologyReplica,
    operation_id: &OperationId,
    build: &PublicBuildInput,
) -> Result<u64, String> {
    use kuberic_runtime::protocol::public_operations::PublicV1RemoteBuildState;

    if replica.report.identity != accepted.identity || replica.report.history != accepted.history {
        return Err("exact removal differs from current source authority".into());
    }
    let remote = replica
        .report
        .remote_builds
        .iter()
        .find(|remote| remote.build == *build && remote.state != PublicV1RemoteBuildState::Dropped)
        .ok_or("exact removal lacks a retained Build")?;
    let is_authority = |kind| {
        !matches!(
            kind,
            PublicV1CallKind::Build | PublicV1CallKind::Close | PublicV1CallKind::Abort
        )
    };
    // Remove also names RetireIdleReplica, whose actual journal class is
    // Authority. Including ordinary exact Remove is safe: its admission
    // revision already equals the then-current retained authority revision,
    // so its completion cannot inflate this high-water.
    let in_scope = |process: &_, storage: &_, history: &_| {
        process == &replica.report.process_session_id
            && storage == &replica.report.storage_incarnation
            && history == &accepted.history
    };
    if let Some(completion) = replica
        .report
        .completed_operations
        .iter()
        .find(|completion| {
            completion.operation_id == *operation_id
                && completion.call_kind == PublicV1CallKind::Remove
                && completion.input_digest == PublicOperationProgram::Remove(build.clone()).digest()
                && in_scope(
                    &completion.process_session_id,
                    &completion.storage_incarnation,
                    &completion.history,
                )
        })
    {
        return Ok(completion.revision);
    }
    if replica.report.active_operations.iter().any(|intent| {
        intent.class.is_terminal()
            || matches!(intent.class, PublicOperationClass::Remove { target }
                if target == build.replica.replica_id && intent.operation_id != *operation_id)
    }) || replica
        .report
        .completed_operations
        .iter()
        .any(|completion| {
            matches!(
                completion.call_kind,
                PublicV1CallKind::Close | PublicV1CallKind::Abort
            ) && in_scope(
                &completion.process_session_id,
                &completion.storage_incarnation,
                &completion.history,
            )
        })
    {
        return Err(
            "exact removal requires resolution of retained terminal or removal ownership".into(),
        );
    }
    let revision = replica
        .report
        .completed_operations
        .iter()
        .filter(|completion| {
            is_authority(completion.call_kind)
                && in_scope(
                    &completion.process_session_id,
                    &completion.storage_incarnation,
                    &completion.history,
                )
        })
        .map(|completion| completion.revision)
        .chain(
            accepted
                .completed_operations
                .iter()
                .filter(|completion| {
                    is_authority(completion.call_kind)
                        && in_scope(
                            &completion.process_session_id,
                            &completion.storage_incarnation,
                            &completion.history,
                        )
                })
                .map(|completion| completion.revision),
        )
        .chain(
            replica
                .report
                .active_operations
                .iter()
                .chain(
                    replica
                        .report
                        .retry_pending_operations
                        .iter()
                        .map(|pending| &pending.intent),
                )
                .filter(|intent| {
                    intent.identity == accepted.identity
                        && intent.process_session_id == replica.report.process_session_id
                        && matches!(
                            intent.class,
                            PublicOperationClass::Authority | PublicOperationClass::PlannedSwap
                        )
                })
                .map(|intent| intent.revision),
        )
        .max()
        .unwrap_or(remote.revision);
    let completed = replica
        .report
        .completed_operations
        .iter()
        .any(|completion| {
            completion.operation_id == remote.operation_id
                && completion.revision == remote.revision
                && completion.call_kind == PublicV1CallKind::Build
                && completion.input_digest == PublicOperationProgram::Build(build.clone()).digest()
                && in_scope(
                    &completion.process_session_id,
                    &completion.storage_incarnation,
                    &completion.history,
                )
        });
    // An active Build can only be superseded at its own revision. Once it is
    // contained/completed, withdrawal instead uses the current source authority.
    let active = replica.report.active_operations.iter().any(|intent| {
        intent.operation_id == remote.operation_id
            && intent.revision == remote.revision
            && intent.program.as_ref() == Some(&PublicOperationProgram::Build(build.clone()))
    });
    if revision < remote.revision || (!completed && (!active || revision != remote.revision)) {
        return Err("retained Build requires containment before current-authority removal".into());
    }
    if replica.report.active_operations.iter().any(|intent| {
        matches!(
            intent.class,
            PublicOperationClass::Build { .. } | PublicOperationClass::Remove { .. }
        ) && intent.revision != revision
    }) {
        return Err("exact removal conflicts with active remote operation authority".into());
    }
    Ok(revision)
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
        Some(
            PublicOperationProgram::Remove(_) | PublicOperationProgram::RetireIdleReplica { .. },
        ) => Ok(PublicV1CallKind::Remove),
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
    include!("public_v1_topology_sync_tests.rs");
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
                engine_idle: None,
                remote_builds: Vec::new(),
                retry_pending_operations: Vec::new(),
                active_operations: Vec::new(),
                configuration: None,
                identity: PublicV1Identity::new(1),
                resource_uid: resource_uid.clone(),
                replica: identity.clone(),
                process_session_id: ProcessSessionId::new(format!("session-{id}")),
                replicator_creation_id: ReplicatorCreationId::new(format!("creation-{id}")),
                report_sequence: 1,
                open_mode: OpenMode::Existing,
                role,
                read_status: if role == ReplicaRole::Primary {
                    AccessStatus::Granted
                } else {
                    AccessStatus::NotPrimary
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
                    current_progress:
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0),
                    catch_up_capability:
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0),
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

    fn fresh_unknown(id: i64) -> PublicV1TopologyReplica {
        use kuberic_runtime::protocol::public_operations::ProgressQueryResult;
        let mut replica = replica(id, ReplicaRole::None);
        replica.report.progress.current_progress = ProgressQueryResult::Value(-1);
        replica.report.progress.catch_up_capability = ProgressQueryResult::Value(-1);
        // Successful Open has already advanced the current reporting mode to
        // Existing. Only the retained exact New-Open digest establishes origin.
        replica.report.completed_operations[0].input_digest = PublicOperationProgram::Open {
            replica: replica.report.replica.clone(),
            existing: false,
        }
        .digest();
        replica
    }

    #[test]
    fn fresh_initial_authority_preserves_unknown_progress_and_singleton_membership() {
        let replicas = vec![fresh_unknown(2), fresh_unknown(1)];
        assert_eq!(
            choose_primary(None, &replicas, None).unwrap(),
            ReplicaId::new(1)
        );
        assert_eq!(replicas[0].report.progress.current(), Some(-1));
        let accepted = accepted(&replicas[0]);
        let next = desired_configuration(
            None,
            &[replicas[1].clone()],
            ReplicaId::new(1),
            accepted.history.data_loss_number,
        )
        .unwrap();
        assert_eq!(next.members.len(), 1);
        assert!(
            matches!(plan_public_v1_topology(&accepted,None,&replicas,2,None,None,"rv").unwrap(),
            PublicV1TopologyPlan::Begin { command, .. } if matches!(command.intent.program,
                Some(PublicOperationProgram::Role { role: ReplicaRole::Primary,.. })))
        );
    }

    #[test]
    fn fresh_initial_authority_rejects_missing_failed_and_retained_provenance() {
        use kuberic_runtime::protocol::public_operations::ProgressQueryResult;
        let fresh = fresh_unknown(1);
        let mut mutations = Vec::new();
        let mut changed = fresh.clone();
        changed.report.completed_operations.clear();
        mutations.push(changed);
        let mut changed = fresh.clone();
        changed.report.completed_operations[0].input_digest = PublicOperationProgram::Open {
            replica: changed.report.replica.clone(),
            existing: true,
        }
        .digest();
        mutations.push(changed);
        let mut changed = fresh.clone();
        changed.report.role = ReplicaRole::IdleSecondary;
        mutations.push(changed);
        let mut changed = fresh.clone();
        changed.report.completed_operations[0].process_session_id = ProcessSessionId::new("old");
        mutations.push(changed);
        let mut changed = fresh.clone();
        changed.report.completed_operations[0]
            .storage_incarnation
            .storage_generation = StorageGeneration::new("old");
        mutations.push(changed);
        let mut changed = fresh.clone();
        changed.report.completed_operations[0].history.history_id =
            kuberic_runtime::protocol::types::HistoryId::new("old");
        mutations.push(changed);
        let mut changed = fresh.clone();
        changed.report.completed_operations[0].result =
            PublicV1OperationResult::Failed("Open failed".into());
        mutations.push(changed);
        let mut changed = fresh.clone();
        let mut prior = changed.report.completed_operations[0].clone();
        prior.call_kind = PublicV1CallKind::Role;
        changed.report.completed_operations.push(prior);
        mutations.push(changed);
        for query in [
            ProgressQueryResult::NotReady,
            ProgressQueryResult::Failed("query failed".into()),
            ProgressQueryResult::NotQueried("unavailable".into()),
        ] {
            let mut changed = fresh.clone();
            changed.report.progress.current_progress = query.clone();
            mutations.push(changed);
            let mut changed = fresh.clone();
            changed.report.progress.catch_up_capability = query;
            mutations.push(changed);
        }
        for changed in mutations {
            assert!(!fresh_unknown_history(&changed));
            assert!(choose_primary(None, &[changed], None).is_err());
        }
        let current = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            ReplicaId::new(2),
            vec![ConfigurationMember {
                identity: replica(2, ReplicaRole::Primary).report.replica,
                role: ReplicaRole::Primary,
            }],
            1,
        );
        assert!(
            choose_primary(Some(&current), std::slice::from_ref(&fresh), None).is_err(),
            "a retained configuration cannot borrow fresh initialization"
        );
        assert!(
            choose_primary(None, &[fresh], Some(ReplicaId::new(1))).is_err(),
            "switchover is not initialization"
        );
    }

    #[test]
    fn fresh_initial_authority_requires_every_exact_participant_through_full_planner() {
        use kuberic_runtime::protocol::public_operations::{
            ProgressQueryResult, PublicV1InstalledConfiguration,
        };
        let primary = fresh_unknown(1);
        let other = fresh_unknown(2);
        let status = accepted(&primary);
        let mut bad = Vec::new();
        let mut changed = other.clone();
        let mut later = changed.report.completed_operations[0].clone();
        later.operation_id = OperationId::new("later-existing-open");
        later.revision += 1;
        later.input_digest = PublicOperationProgram::Open {
            replica: changed.report.replica.clone(),
            existing: true,
        }
        .digest();
        changed.report.completed_operations.push(later);
        bad.push(changed);
        let mut changed = other.clone();
        let program = PublicOperationProgram::Role {
            epoch: Epoch::new(0, 1),
            role: ReplicaRole::Primary,
        };
        changed.report.active_operations.push(
            kuberic_runtime::protocol::public_operations::PublicOperationIntent {
                identity: changed.report.identity.clone(),
                operation_id: OperationId::new("active-role"),
                revision: 2,
                process_session_id: changed.report.process_session_id.clone(),
                class:
                    kuberic_runtime::protocol::public_operations::PublicOperationClass::Authority,
                input_digest: program.digest(),
                replay_disposition: program.replay_disposition(),
                lifecycle: None,
                program: Some(program),
            },
        );
        bad.push(changed);
        let mut changed = other.clone();
        changed.report.configuration = Some(PublicV1InstalledConfiguration {
            voting: PublicConfiguration {
                current: ConfigurationDescriptor::new(
                    Epoch::new(0, 1),
                    ReplicaId::new(2),
                    vec![ConfigurationMember {
                        identity: changed.report.replica.clone(),
                        role: ReplicaRole::Primary,
                    }],
                    1,
                ),
                previous: None,
                replicas: vec![],
            },
        });
        bad.push(changed);
        let mut changed = other.clone();
        changed.report.history.history_id =
            kuberic_runtime::protocol::types::HistoryId::new("different-report-history");
        bad.push(changed);
        let mut changed = other.clone();
        changed.report.history.protocol_generation = PublicProtocolGeneration::new(2);
        bad.push(changed);
        let mut changed = other.clone();
        changed.report.completed_operations[0].history.history_id =
            kuberic_runtime::protocol::types::HistoryId::new("different-open-history");
        bad.push(changed);
        let mut changed = other.clone();
        changed.report.replica.instance_id = ReplicaInstanceId::new("different-pod");
        changed.report.storage_incarnation.replica = changed.report.replica.clone();
        bad.push(changed);
        let mut changed = other.clone();
        changed.report.replica.agent_generation = AgentGeneration::new("different-generation");
        changed.report.storage_incarnation.replica = changed.report.replica.clone();
        bad.push(changed);
        let mut changed = other.clone();
        changed.report.storage_incarnation.storage_generation =
            StorageGeneration::new("different-storage-generation");
        bad.push(changed);
        let mut changed = other.clone();
        changed.report.storage_incarnation.storage = ReplicaStorageIdentity::Persistent {
            pod_uid: PodUid::new("other-pod"),
            pvc_uid: PvcUid::new("other-pvc"),
        };
        bad.push(changed);
        let mut changed = other.clone();
        changed.report.completed_operations[0].result = PublicV1OperationResult::Cancelled;
        bad.push(changed);
        let mut changed = other.clone();
        changed.report.completed_operations[0].result =
            PublicV1OperationResult::Ambiguous("uncertain Open".into());
        bad.push(changed);
        let mut changed = other.clone();
        changed.report.progress.current_progress = ProgressQueryResult::Value(-2);
        bad.push(changed);
        for capability in [0, 99, -2] {
            let mut changed = other.clone();
            changed.report.progress.catch_up_capability = ProgressQueryResult::Value(capability);
            bad.push(changed);
        }
        for changed in bad {
            let result = plan_public_v1_topology(
                &status,
                None,
                &[primary.clone(), changed],
                2,
                None,
                None,
                "rv",
            );
            assert!(
                matches!(result, Err(_) | Ok(PublicV1TopologyPlan::Wait))
                    || matches!(result,Ok(PublicV1TopologyPlan::Begin {command,..}) if matches!(command.intent.program,Some(PublicOperationProgram::Open {..}))),
                "nonselected bad participant authorized initial role/configuration"
            );
        }
        for current in [0, 99] {
            let mut changed = other.clone();
            changed.report.progress.current_progress = ProgressQueryResult::Value(current);
            assert!(!fresh_unknown_history(&changed));
            assert_eq!(
                choose_primary(None, &[primary.clone(), changed], None).unwrap(),
                ReplicaId::new(2),
                "legitimate ordinary nonnegative selection was changed by initialization exception"
            );
        }
        let mut crossed = status.clone();
        crossed.history.history_id =
            kuberic_runtime::protocol::types::HistoryId::new("different-accepted-history");
        assert!(matches!(
            plan_public_v1_topology(&crossed, None, &[primary, other], 2, None, None, "rv"),
            Ok(PublicV1TopologyPlan::Wait)
        ));
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

    #[test]
    fn availability_bindings_refresh_exact_previous_only_observations() {
        let source = replica(1, ReplicaRole::Primary);
        let peer = replica(2, ReplicaRole::ActiveSecondary);
        let previous = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            source.report.replica.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.report.replica.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: peer.report.replica.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            1,
        );
        let current = ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            source.report.replica.replica_id,
            vec![ConfigurationMember {
                identity: source.report.replica.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        let observed = vec![source.clone(), peer.clone()];
        let known =
            native_public_configuration(&source.report.replica, &observed, &previous, None, None)
                .unwrap();
        assert!(known.replicas[0].is_up && known.replicas[0].is_ready);
        for up in [false, true] {
            let mut changed = known.clone();
            if up {
                changed.replicas[0].is_up = false;
            } else {
                changed.replicas[0].is_ready = false;
            }
            assert!(!same_bindings(&known, &changed));
        }
        let exact = native_public_configuration(
            &source.report.replica,
            &observed,
            &current,
            Some(&previous),
            Some(&known),
        )
        .unwrap();
        assert!(exact.replicas[0].is_up && exact.replicas[0].is_ready);
        assert_eq!(
            exact.replicas[0].current_progress,
            kuberic_runtime::protocol::public_operations::INVALID_LSN
        );
        let absent = native_public_configuration(
            &source.report.replica,
            std::slice::from_ref(&source),
            &current,
            Some(&previous),
            Some(&known),
        )
        .unwrap();
        assert!(!absent.replicas[0].is_up && !absent.replicas[0].is_ready);
        let mut crossed = observed.clone();
        crossed[1].report.process_session_id = ProcessSessionId::new("new-peer-process");
        let stale = native_public_configuration(
            &source.report.replica,
            &crossed,
            &current,
            Some(&previous),
            Some(&known),
        )
        .unwrap();
        assert!(!stale.replicas[0].is_up && !stale.replicas[0].is_ready);
        let mut not_ready = observed;
        not_ready[1].report.progress.current_progress =
            kuberic_runtime::protocol::public_operations::ProgressQueryResult::NotReady;
        let unavailable = native_public_configuration(
            &source.report.replica,
            &not_ready,
            &current,
            Some(&previous),
            Some(&known),
        )
        .unwrap();
        assert!(unavailable.replicas[0].is_up && !unavailable.replicas[0].is_ready);
    }

    #[test]
    fn availability_current_peer_producers_preserve_unknown_progress_and_exact_authority() {
        use kuberic_runtime::protocol::public_operations::{INVALID_LSN, ProgressQueryResult};
        let source = replica(1, ReplicaRole::Primary);
        let peer = replica(2, ReplicaRole::ActiveSecondary);
        let current = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            source.report.replica.replica_id,
            vec![
                ConfigurationMember {
                    identity: source.report.replica.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: peer.report.replica.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        for (current_ready, capability_ready) in
            [(false, true), (true, false), (false, false), (true, true)]
        {
            let mut observed = vec![source.clone(), peer.clone()];
            observed[1].report.progress.current_progress = if current_ready {
                ProgressQueryResult::Value(0)
            } else {
                ProgressQueryResult::NotReady
            };
            observed[1].report.progress.catch_up_capability = if capability_ready {
                ProgressQueryResult::Value(0)
            } else {
                ProgressQueryResult::NotReady
            };
            observed[1].report.validate().unwrap();
            for input in [
                public_configuration(&source.report.replica, &observed, &current).unwrap(),
                native_public_configuration(
                    &source.report.replica,
                    &observed,
                    &current,
                    None,
                    None,
                )
                .unwrap(),
            ] {
                input.validate().unwrap();
                let descriptor = &input.replicas[0];
                assert_eq!(descriptor.identity, peer.report.replica);
                assert_eq!(
                    descriptor.process_session_id,
                    peer.report.process_session_id
                );
                assert_eq!(
                    descriptor.storage_incarnation.as_ref(),
                    Some(&peer.report.storage_incarnation)
                );
                assert_eq!(descriptor.replication_address, peer.replication_address);
                assert!(descriptor.is_up);
                assert_eq!(descriptor.is_ready, current_ready && capability_ready);
                assert_eq!(
                    descriptor.current_progress,
                    if current_ready { 0 } else { INVALID_LSN }
                );
                assert_eq!(
                    descriptor.catch_up_capability,
                    if capability_ready { 0 } else { INVALID_LSN }
                );
            }
        }
        assert!(
            public_configuration(
                &source.report.replica,
                std::slice::from_ref(&source),
                &current
            )
            .is_err()
        );
        assert!(
            native_public_configuration(
                &source.report.replica,
                std::slice::from_ref(&source),
                &current,
                None,
                None
            )
            .is_err()
        );
    }

    #[test]
    fn availability_full_planner_refreshes_deterioration_and_restoration_before_stable() {
        let mut replicas = vec![
            replica(1, ReplicaRole::Primary),
            replica(2, ReplicaRole::ActiveSecondary),
            replica(3, ReplicaRole::ActiveSecondary),
        ];
        let desired = desired_configuration(None, &replicas, ReplicaId::new(1), 0).unwrap();
        let mut authority = accepted(&replicas[0]);
        for revision in 2..24 {
            let Some((target, program)) = native_configuration_step(&replicas, &desired).unwrap()
            else {
                break;
            };
            let session = replicas
                .iter()
                .find(|replica| replica.report.replica == target)
                .unwrap()
                .report
                .process_session_id
                .clone();
            accept_command(
                &mut authority,
                &mut replicas,
                &PublicV1OperationCommand {
                    target,
                    intent: PublicOperationIntent {
                        identity: PublicV1Identity::new(1),
                        operation_id: OperationId::new(format!("availability-baseline-{revision}")),
                        revision,
                        process_session_id: session,
                        class: PublicOperationClass::Authority,
                        input_digest: program.digest(),
                        replay_disposition: program.replay_disposition(),
                        lifecycle: None,
                        program: Some(program),
                    },
                },
            );
        }
        assert!(
            native_configuration_step(&replicas, &desired)
                .unwrap()
                .is_none()
        );
        let plan = |authority: &PublicV1AcceptedStatus, replicas: &[PublicV1TopologyReplica]| {
            plan_public_v1_topology(
                authority,
                Some(&desired),
                replicas,
                3,
                None,
                Some(desired.primary_id),
                "availability-resource-version",
            )
            .unwrap()
        };
        assert_eq!(plan(&authority, &replicas), PublicV1TopologyPlan::Stable);

        for (missing_current, missing_capability) in [(true, false), (false, true), (true, true)] {
            for ready in [false, true] {
                let expected = if ready {
                    PublicV1TopologyPlan::Stable
                } else {
                    PublicV1TopologyPlan::Wait
                };
                for peer in &mut replicas[1..] {
                    peer.report.progress.current_progress = if ready || !missing_current {
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0)
                    } else {
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::NotReady
                    };
                    peer.report.progress.catch_up_capability = if ready || !missing_capability {
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0)
                    } else {
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::NotReady
                    };
                    peer.report.validate().unwrap();
                }
                assert!(matches!(
                    plan(&authority, &replicas),
                    PublicV1TopologyPlan::Begin { .. }
                ));
                let mut converged = false;
                for _ in 0..24 {
                    match plan(&authority, &replicas) {
                        PublicV1TopologyPlan::Begin { command, .. } => {
                            assert!(matches!(
                                command.intent.program,
                                Some(
                                    PublicOperationProgram::Configuration(_)
                                        | PublicOperationProgram::CatchUp { .. }
                                )
                            ));
                            accept_command(&mut authority, &mut replicas, &command);
                            let primary = &mut replicas[0].report;
                            let installed = &primary.configuration.as_ref().unwrap().voting;
                            primary.write_status = if installed
                                .replicas
                                .iter()
                                .any(|peer| peer.is_up && peer.is_ready)
                            {
                                AccessStatus::Granted
                            } else {
                                AccessStatus::NoWriteQuorum
                            };
                        }
                        terminal if terminal == expected => {
                            converged = true;
                            break;
                        }
                        other => panic!("availability-only work did not converge: {other:?}"),
                    }
                }
                assert!(converged, "availability-only work never converged");
                assert!(
                    native_configuration_step(&replicas, &desired)
                        .unwrap()
                        .is_none()
                );
                for replica in &replicas {
                    let installed = &replica.report.configuration.as_ref().unwrap().voting;
                    assert_eq!(installed.current, desired);
                    assert!(installed.previous.is_none());
                    for peer in &installed.replicas {
                        assert!(peer.is_up);
                        assert_eq!(
                            peer.is_ready,
                            ready || peer.identity.replica_id == desired.primary_id
                        );
                    }
                }
                assert_eq!(plan(&authority, &replicas), expected);
            }
        }
    }

    #[test]
    fn availability_only_reconfiguration_converges_and_secondary_promotion_requires_catchup() {
        let mut replicas = vec![
            replica(1, ReplicaRole::Primary),
            replica(2, ReplicaRole::ActiveSecondary),
        ];
        let desired = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            replicas[0].report.replica.replica_id,
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
            1,
        );
        let mut authority = accepted(&replicas[0]);
        let mut revision = 2;
        for _ in 0..10 {
            let Some((target, program)) = native_configuration_step(&replicas, &desired).unwrap()
            else {
                break;
            };
            let session = replicas
                .iter()
                .find(|replica| replica.report.replica == target)
                .unwrap()
                .report
                .process_session_id
                .clone();
            let command = PublicV1OperationCommand {
                target,
                intent: PublicOperationIntent {
                    identity: authority.identity.clone(),
                    operation_id: OperationId::new(format!("available-{revision}")),
                    revision,
                    process_session_id: session,
                    class: PublicOperationClass::Authority,
                    input_digest: program.digest(),
                    replay_disposition: program.replay_disposition(),
                    lifecycle: None,
                    program: Some(program),
                },
            };
            accept_command(&mut authority, &mut replicas, &command);
            revision += 1;
        }
        assert!(
            native_configuration_step(&replicas, &desired)
                .unwrap()
                .is_none()
        );
        replicas[0]
            .report
            .configuration
            .as_mut()
            .unwrap()
            .voting
            .replicas[0]
            .is_ready = false;
        assert!(
            native_configuration_step(&replicas, &desired)
                .unwrap()
                .is_some()
        );
        for _ in 0..10 {
            let Some((target, program)) = native_configuration_step(&replicas, &desired).unwrap()
            else {
                break;
            };
            let session = replicas
                .iter()
                .find(|replica| replica.report.replica == target)
                .unwrap()
                .report
                .process_session_id
                .clone();
            let command = PublicV1OperationCommand {
                target,
                intent: PublicOperationIntent {
                    identity: authority.identity.clone(),
                    operation_id: OperationId::new(format!("available-{revision}")),
                    revision,
                    process_session_id: session,
                    class: PublicOperationClass::Authority,
                    input_digest: program.digest(),
                    replay_disposition: program.replay_disposition(),
                    lifecycle: None,
                    program: Some(program),
                },
            };
            accept_command(&mut authority, &mut replicas, &command);
            revision += 1;
        }
        assert!(
            native_configuration_step(&replicas, &desired)
                .unwrap()
                .is_none()
        );
        assert!(
            replicas[0]
                .report
                .configuration
                .as_ref()
                .unwrap()
                .voting
                .replicas[0]
                .is_ready
        );
        let promoted = ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            replicas[1].report.replica.replica_id,
            vec![ConfigurationMember {
                identity: replicas[1].report.replica.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        );
        let sequence =
            operation_sequence(&replicas[1], &replicas, &promoted, false, false, false).unwrap();
        assert!(
            sequence
                .iter()
                .any(|(_, program)| matches!(program, PublicOperationProgram::CatchUp { .. }))
        );
        assert!(
            !sequence
                .iter()
                .any(|(_, program)| matches!(program, PublicOperationProgram::Configuration(_)))
        );
    }

    #[test]
    fn topology_pending_retry_retains_revision_instead_of_inventing_completion() {
        use kuberic_runtime::protocol::public_operations::PublicV1RetryPendingOperation;
        let mut source = replica(1, ReplicaRole::Primary);
        let authority = accepted(&source);
        let program = PublicOperationProgram::Epoch {
            epoch: Epoch::new(0, 2),
        };
        let intent = PublicOperationIntent {
            identity: authority.identity.clone(),
            operation_id: OperationId::new("pending-epoch"),
            revision: 7,
            process_session_id: source.report.process_session_id.clone(),
            class: PublicOperationClass::Authority,
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program.clone()),
        };
        source
            .report
            .retry_pending_operations
            .push(PublicV1RetryPendingOperation {
                intent: intent.clone(),
                attempt: 1,
                failure: "declared quiescent epoch failure".into(),
            });
        source.report.validate().unwrap();
        let (_, retry) = begin_operation(
            &authority,
            &source,
            program.clone(),
            intent.operation_id.clone(),
            "pending-retry",
        )
        .unwrap();
        assert_eq!(retry.intent, intent);
        assert_eq!(
            operation_revision(
                &authority,
                &source,
                &OperationId::new("new-operation"),
                &program,
                "new",
            )
            .unwrap(),
            8
        );
        assert!(
            operation_revision(
                &authority,
                &source,
                &intent.operation_id,
                &PublicOperationProgram::Epoch {
                    epoch: Epoch::new(0, 3)
                },
                "changed",
            )
            .is_err()
        );
        assert!(
            source
                .report
                .completed_operations
                .iter()
                .all(|completed| { completed.operation_id != intent.operation_id })
        );
    }

    #[test]
    fn generic_clean_formation_does_not_require_engine_idle_diagnostic() {
        let mut replicas = vec![replica(1, ReplicaRole::None)];
        let mut authority = accepted(&replicas[0]);
        let mut current = None;
        let mut routed = None;
        let mut stable = false;
        for _ in 0..24 {
            assert!(replicas[0].report.engine_idle.is_none());
            assert!(replicas[0].report.remote_builds.is_empty());
            match plan_public_v1_topology(
                &authority,
                current.as_ref(),
                &replicas,
                1,
                None,
                routed,
                "generic-formation",
            )
            .unwrap()
            {
                PublicV1TopologyPlan::Begin { command, .. } => {
                    accept_command(&mut authority, &mut replicas, &command);
                }
                PublicV1TopologyPlan::PersistTopology { next, .. } => current = Some(*next),
                PublicV1TopologyPlan::RoutePrimary { primary } => routed = Some(primary.replica_id),
                PublicV1TopologyPlan::Stable => {
                    stable = true;
                    break;
                }
                PublicV1TopologyPlan::Wait => panic!("optional diagnostic blocked clean formation"),
            }
        }
        assert!(stable, "generic singleton did not form");
        assert_eq!(current.unwrap().members.len(), 1);
        assert!(replicas[0].report.configuration.is_some());
        assert!(
            replicas[0].report.engine_idle.is_none(),
            "host installation is not an empty query"
        );
    }

    #[test]
    fn cold_copy_separates_optional_idle_inference_from_exact_retirement() {
        use kuberic_runtime::protocol::public_operations::{
            PublicV1EngineIdleObservation, PublicV1InstalledConfiguration,
            PublicV1RemoteBuildObservation, PublicV1RemoteBuildState,
        };
        let mut replicas = vec![
            replica(1, ReplicaRole::Primary),
            replica(2, ReplicaRole::IdleSecondary),
        ];
        let installed = desired_configuration(None, &replicas[..1], ReplicaId::new(1), 0).unwrap();
        let voting = native_public_configuration(
            &replicas[0].report.replica,
            &replicas,
            &installed,
            None,
            None,
        )
        .unwrap();
        replicas[0].report.configuration = Some(PublicV1InstalledConfiguration {
            voting: voting.clone(),
        });
        let step = |replicas: &[PublicV1TopologyReplica]| {
            native_cold_copy_step(replicas, &replicas[0], &replicas[1], installed.epoch).unwrap()
        };
        assert!(step(&replicas).is_none());
        assert!(replicas[0].report.engine_idle.is_none());
        replicas[0].report.engine_idle = Some(PublicV1EngineIdleObservation {
            configuration: voting,
            history: replicas[0].report.history.clone(),
            storage_incarnation: replicas[0].report.storage_incarnation.clone(),
            process_session_id: replicas[0].report.process_session_id.clone(),
            replicator_creation_id: replicas[0].report.replicator_creation_id.clone(),
            idle_replicas: Vec::new(),
        });
        assert!(step(&replicas).is_none());
        replicas[0]
            .report
            .engine_idle
            .as_mut()
            .unwrap()
            .idle_replicas
            .push(ReplicaId::new(2));
        assert!(
            matches!(step(&replicas), Some((_, PublicOperationProgram::RetireIdleReplica { replica_id })) if replica_id == ReplicaId::new(2))
        );
        let build = PublicBuildInput {
            attempt: OperationId::new("uncertain-copy"),
            epoch: installed.epoch,
            current_configuration_id: Some(installed.configuration_id),
            replica: replicas[1].report.replica.clone(),
            target_storage: replicas[1].report.storage_incarnation.clone(),
            process_session_id: replicas[1].report.process_session_id.clone(),
            source_address: replicas[0].replication_address.clone(),
            replication_address: replicas[1].replication_address.clone(),
            history: Some(replicas[0].report.history.clone()),
        };
        replicas[0]
            .report
            .remote_builds
            .push(PublicV1RemoteBuildObservation {
                operation_id: build.attempt.clone(),
                revision: 2,
                build: build.clone(),
                state: PublicV1RemoteBuildState::InDrop,
            });
        for diagnostic in [true, false] {
            if !diagnostic {
                replicas[0].report.engine_idle = None;
            }

            replicas[0].report.validate().unwrap();
            assert_eq!(
                step(&replicas),
                Some((
                    replicas[0].report.replica.clone(),
                    PublicOperationProgram::Remove(build.clone())
                )),
                "known exact uncertainty must not become unowned idle inference"
            );
        }
        replicas[0].report.remote_builds[0].state = PublicV1RemoteBuildState::Dropped;
        assert!(step(&replicas).is_none());
        assert!(
            replicas[0].report.engine_idle.is_none(),
            "exact retirement did not perform a query"
        );
        replicas[0].report.remote_builds[0].state = PublicV1RemoteBuildState::Ready;
        let program = PublicOperationProgram::Build(build.clone());
        let report = &mut replicas[0].report;
        report
            .completed_operations
            .push(PublicV1OperationCompletion {
                operation_id: build.attempt.clone(),
                revision: 2,
                call_kind: PublicV1CallKind::Build,
                process_session_id: report.process_session_id.clone(),
                input_digest: program.digest(),
                replay_disposition: program.replay_disposition(),
                history: report.history.clone(),
                storage_incarnation: report.storage_incarnation.clone(),
                previous_configuration_id: None,
                current_configuration_id: build.current_configuration_id.clone(),
                catch_up_mode: None,
                catch_up_specific_quorum: None,
                result: PublicV1OperationResult::Succeeded,
            });
        report.validate().unwrap();
        assert_eq!(
            step(&replicas),
            Some((
                replicas[0].report.replica.clone(),
                PublicOperationProgram::Remove(build)
            )),
            "an exact unretired Ready owner is withdrawn without inferring native membership"
        );
        assert!(replicas[0].report.engine_idle.is_none());
        replicas[0].report.remote_builds[0].state = PublicV1RemoteBuildState::Dropped;
        assert!(
            step(&replicas).is_none(),
            "retired success cannot become renewed ownership"
        );
        replicas[0].report.remote_builds.clear();
        assert!(
            step(&replicas).is_none(),
            "historical completion alone is not native idle membership"
        );
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
                report.report.open_mode = OpenMode::Existing;
                if !report.replication_address.is_empty() {
                    report.report.replication_address = Some(report.replication_address.clone());
                }
                (PublicV1CallKind::Open, None, None, None, None)
            }
            PublicOperationProgram::Role { role, .. } => {
                report.report.role = *role;
                report.report.progress.role = match report.report.role {
                    ReplicaRole::Primary => PublicProgressRole::Primary,
                    ReplicaRole::ActiveSecondary | ReplicaRole::IdleSecondary => {
                        PublicProgressRole::Secondary
                    }
                    ReplicaRole::None => PublicProgressRole::Recovery,
                };
                report.report.read_status = AccessStatus::NotPrimary;
                report.report.write_status = AccessStatus::NotPrimary;
                (PublicV1CallKind::Role, None, None, None, None)
            }
            PublicOperationProgram::Epoch { .. } => {
                (PublicV1CallKind::Epoch, None, None, None, None)
            }
            PublicOperationProgram::Configuration(configuration) => {
                report.report.configuration = Some(
                    kuberic_runtime::protocol::public_operations::PublicV1InstalledConfiguration {
                        voting: configuration.clone(),
                    },
                );
                report.report.read_status = if report.report.role == ReplicaRole::Primary {
                    AccessStatus::Granted
                } else {
                    AccessStatus::NotPrimary
                };
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
                report.report.configuration = Some(
                    kuberic_runtime::protocol::public_operations::PublicV1InstalledConfiguration {
                        voting: configuration.clone(),
                    },
                );
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
        if let Some(existing) = report
            .report
            .completed_operations
            .iter()
            .find(|existing| existing.operation_id == completion.operation_id)
        {
            assert_eq!(existing, &completion);
        } else {
            report.report.completed_operations.push(completion.clone());
        }
        let completion = PublicV1AcceptedOperationCompletion::from_completion(&completion);
        if !accepted.completed_operations.contains(&completion) {
            accepted.completed_operations.push(completion);
        }
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
        replicas[0].report.open_mode = OpenMode::New;
        replicas[2].report.open_mode = OpenMode::New;
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
                    existing: replicas[index].report.open_mode == OpenMode::Existing,
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
        for mode in [OpenMode::New, OpenMode::Existing] {
            let mut replicas = vec![
                replica(1, ReplicaRole::Primary),
                replica(2, ReplicaRole::ActiveSecondary),
                replica(3, ReplicaRole::ActiveSecondary),
            ];
            let current = desired_configuration(None, &replicas, ReplicaId::new(1), 0).unwrap();
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
        let original = desired_configuration(None, &replicas, ReplicaId::new(1), 0).unwrap();
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
        replicas[0].report.progress.catch_up_capability =
            kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(1);
        replicas[0].report.progress.current_progress =
            kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(1);
        replicas[1] = replica(2, ReplicaRole::Primary);
        replicas[1].report.progress.catch_up_capability =
            kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(2);
        replicas[1].report.progress.current_progress =
            kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(2);
        let current =
            desired_configuration(Some(&original), &replicas[1..], ReplicaId::new(2), 0).unwrap();
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
                3 => {
                    target.report.progress.current_progress =
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::NotReady
                }
                4 => target.report.completed_operations.clear(),
                5 => evidence.completed_operations.clear(),
                6 => {
                    target.report.progress.catch_up_capability =
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(3);
                    target.report.progress.current_progress =
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(3);
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
        fresh_primary.report.completed_operations.clear();
        fresh_primary.report.replication_address = None;
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
