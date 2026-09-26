use std::collections::BTreeSet;

use crate::command::EnsureConfiguration;
use crate::types::*;
use crate::validation::{ValidationError, validate_configuration, validate_policy};

type Result<T = ()> = std::result::Result<T, ValidationError>;

fn invalid(reason: &'static str) -> ValidationError {
    ValidationError::InvalidScaleUp(reason)
}

pub fn validate_scale_up_provisioning(intent: &ProvisioningIntent) -> Result {
    if intent.pod_uid.is_empty()
        || intent.pvc_uid.is_empty()
        || intent.operation_id.is_empty()
        || intent.replica_id().value() <= 0
    {
        return Err(invalid("invalid candidate identity"));
    }
    match intent.purpose.kind {
        ProvisioningKind::Replacement => {
            let Some(replaces) = intent.purpose.replaces.as_ref() else {
                return Err(invalid("replacement provisioning purpose is malformed"));
            };
            if intent.purpose.scale_up.is_some()
                || replaces.replica_id.value() <= 0
                || replaces.instance_id.is_empty()
                || replaces.agent_generation.is_empty()
            {
                return Err(invalid("replacement provisioning purpose is malformed"));
            }
        }
        ProvisioningKind::ScaleUp
            if intent.purpose.replaces.is_some() || intent.purpose.scale_up.is_none() =>
        {
            return Err(invalid("scale-up provisioning purpose is malformed"));
        }
        _ => {}
    }
    let Some(scale_up) = intent.scale_up() else {
        return Ok(());
    };
    validate_scale_up_provisioning_request(scale_up)?;
    if intent.operation_id != intent.expected_operation_id() {
        return Err(invalid("invalid frozen provisioning request"));
    }
    Ok(())
}

pub fn validate_scale_up_provisioning_request(scale_up: &ScaleUpProvisioning) -> Result {
    validate_policy(&scale_up.previous_policy)?;
    validate_policy(&scale_up.current_policy)?;
    validate_configuration(
        &scale_up.previous_configuration,
        Some(&scale_up.previous_policy),
    )?;
    if scale_up.resource_uid.is_empty()
        || scale_up.spec_generation == 0
        || scale_up.desired_replicas == 0
        || scale_up.previous_policy.replica_set_size == u32::MAX
        || scale_up.current_policy.replica_set_size != scale_up.previous_policy.replica_set_size + 1
        || scale_up.desired_replicas < scale_up.current_policy.replica_set_size
        || scale_up.previous_policy.failover_delay_seconds
            != scale_up.current_policy.failover_delay_seconds
        || scale_up.target_replica_id.value() != i64::from(scale_up.current_policy.replica_set_size)
        || scale_up
            .previous_configuration
            .members
            .iter()
            .any(|member| member.identity.replica_id == scale_up.target_replica_id)
        || scale_up.next_configuration_epoch().is_none()
    {
        return Err(invalid("invalid frozen provisioning request"));
    }
    let ids = scale_up
        .previous_configuration
        .members
        .iter()
        .map(|member| member.identity.replica_id.value())
        .collect::<BTreeSet<_>>();
    let expected =
        (1..=i64::from(scale_up.previous_policy.replica_set_size)).collect::<BTreeSet<_>>();
    if ids != expected {
        return Err(invalid(
            "scale-up requires contiguous cardinality-derived logical IDs",
        ));
    }
    Ok(())
}

pub fn validate_scale_up(intent: &ScaleUpIntent) -> Result {
    validate_policy(&intent.previous_policy)?;
    validate_policy(&intent.current_policy)?;
    validate_configuration(
        &intent.previous_configuration,
        Some(&intent.previous_policy),
    )?;
    validate_configuration(&intent.current_configuration, Some(&intent.current_policy))?;
    if intent.resource_uid.is_empty()
        || intent.spec_generation == 0
        || intent.desired_replicas == 0
        || intent.build_id.is_empty()
        || intent.primary.instance_id.is_empty()
        || intent.primary.agent_generation.is_empty()
        || intent.target.instance_id.is_empty()
        || intent.target.agent_generation.is_empty()
        || intent.snapshot_boundary_lsn < 0
        || intent.catch_up_boundary_lsn < intent.snapshot_boundary_lsn
        || intent.operation_id != intent.expected_operation_id()
    {
        return Err(invalid("invalid frozen request, build, or boundary"));
    }
    let previous = &intent.previous_configuration;
    let current = &intent.current_configuration;
    if intent.previous_policy.replica_set_size == u32::MAX
        || intent.current_policy.replica_set_size != intent.previous_policy.replica_set_size + 1
        || intent.desired_replicas < intent.current_policy.replica_set_size
        || intent.previous_policy.failover_delay_seconds
            != intent.current_policy.failover_delay_seconds
        || previous.members.len() + 1 != current.members.len()
    {
        return Err(invalid(
            "scale-up must preserve delay and increase membership by exactly one",
        ));
    }
    if previous.epoch.data_loss_number < 0
        || previous.epoch.configuration_number < 0
        || current.epoch.data_loss_number != previous.epoch.data_loss_number
    {
        return Err(ValidationError::TransitionDataLossChanged);
    }
    if previous
        .epoch
        .configuration_number
        .checked_add(1)
        .is_none_or(|next| current.epoch.configuration_number != next)
    {
        return Err(ValidationError::TransitionEpochNotNewer);
    }
    let primary = previous
        .members
        .iter()
        .find(|member| member.role == ReplicaRole::Primary)
        .expect("validated configuration has a primary");
    let added = current
        .members
        .iter()
        .filter(|member| {
            !previous
                .members
                .iter()
                .any(|old| old.identity.replica_id == member.identity.replica_id)
        })
        .collect::<Vec<_>>();
    let previous_ids = previous
        .members
        .iter()
        .map(|member| member.identity.replica_id.value())
        .collect::<BTreeSet<_>>();
    let expected_previous_ids =
        (1..=i64::from(intent.previous_policy.replica_set_size)).collect::<BTreeSet<_>>();
    if intent.primary != primary.identity
        || previous.primary_id != current.primary_id
        || added.len() != 1
        || added[0].identity != intent.target
        || added[0].role != ReplicaRole::ActiveSecondary
        || intent.target.replica_id.value() != i64::from(intent.current_policy.replica_set_size)
        || previous
            .members
            .iter()
            .any(|member| !current.members.iter().any(|candidate| candidate == member))
        || previous_ids != expected_previous_ids
    {
        return Err(invalid(
            "target must be the next active secondary and retained authority cannot change",
        ));
    }
    Ok(())
}

fn validate_witnesses(
    intent: &ScaleUpIntent,
    eligible_members: &ConfigurationDescriptor,
    witnesses: &[ScaleUpWitness],
    quorum: u32,
    previous_current: bool,
    require_primary: bool,
) -> Result {
    let mut identities = BTreeSet::new();
    for witness in witnesses {
        let current_member = intent
            .current_configuration
            .members
            .iter()
            .find(|member| member.identity == witness.identity);
        if witness.resource_uid != intent.resource_uid
            || witness.process_session_id.is_empty()
            || witness.report_sequence == 0
            || !eligible_members
                .members
                .iter()
                .any(|member| member.identity == witness.identity)
            || current_member.is_none_or(|member| member.role != witness.role)
            || !identities.insert(witness.identity.clone())
            || witness.epoch != intent.current_configuration.epoch
            || witness.previous_configuration_id.as_ref()
                != previous_current.then_some(&intent.previous_configuration.configuration_id)
            || witness.current_configuration_id != intent.current_configuration.configuration_id
            || witness.verified_replication_lsn < intent.catch_up_boundary_lsn
            || witness.pending_operation_id.is_some()
            || (witness.write_status == AccessStatus::Granted && witness.identity != intent.primary)
            || (!previous_current
                && witness.identity == intent.primary
                && witness.write_status != AccessStatus::Granted)
            || witness.retained_operation_id.as_ref()
                != Some(&intent.command_operation_id(
                    if previous_current {
                        ScaleUpStage::PreviousCurrent
                    } else {
                        ScaleUpStage::CurrentOnly
                    },
                    &witness.identity,
                    &intent.current_configuration,
                ))
        {
            return Err(invalid("invalid exact scale-up quorum witness"));
        }
    }
    if identities.len() < quorum as usize
        || (require_primary && !identities.contains(&intent.primary))
    {
        return Err(invalid("insufficient scale-up quorum witnesses"));
    }
    Ok(())
}

pub fn validate_scale_up_failover_evidence(evidence: &ScaleUpFailoverEvidence) -> Result {
    validate_scale_up(&evidence.intent)?;
    validate_witnesses(
        &evidence.intent,
        &evidence.intent.previous_configuration,
        &evidence.previous_read_quorum,
        evidence.intent.previous_policy.read_quorum,
        true,
        false,
    )?;
    validate_witnesses(
        &evidence.intent,
        &evidence.intent.current_configuration,
        &evidence.current_read_quorum,
        evidence.intent.current_policy.read_quorum,
        true,
        false,
    )
}

pub fn validate_scale_up_failover_transition(
    evidence: &ScaleUpFailoverEvidence,
    current: &ConfigurationDescriptor,
    policy: &EffectivePolicy,
) -> Result {
    validate_scale_up_failover_evidence(evidence)?;
    validate_policy(policy)?;
    validate_configuration(current, Some(policy))?;
    if policy != &evidence.intent.current_policy
        || current.epoch.data_loss_number
            != evidence.intent.current_configuration.epoch.data_loss_number
        || current.epoch.configuration_number
            <= evidence
                .intent
                .current_configuration
                .epoch
                .configuration_number
    {
        return Err(invalid("failover authority did not advance expanded CC"));
    }
    let expected = evidence
        .intent
        .current_configuration
        .members
        .iter()
        .map(|member| member.identity.clone())
        .collect::<BTreeSet<_>>();
    let actual = current
        .members
        .iter()
        .map(|member| member.identity.clone())
        .collect::<BTreeSet<_>>();
    if actual != expected {
        return Err(invalid("failover must preserve exact expanded membership"));
    }
    Ok(())
}

pub fn validate_scale_up_cleanup(cleanup: &ScaleUpCleanup) -> Result {
    validate_scale_up_provisioning(&cleanup.provisioning)?;
    let Some(scale_up) = cleanup.provisioning.scale_up() else {
        return Err(invalid("cleanup requires scale-up provisioning"));
    };
    let target = cleanup.provisioning.target_identity(&scale_up.resource_uid);
    if cleanup.target != target
        || !matches!(&cleanup.resources.pod, CleanupResourceIdentity::Present { uid, .. } if uid == cleanup.provisioning.pod_uid.as_str())
        || !matches!(&cleanup.resources.pvc, CleanupResourceIdentity::Present { uid, .. } if uid == cleanup.provisioning.pvc_uid.as_str())
        || cleanup.resources.endpoint.name()
            != derive_replica_endpoint_name(&scale_up.resource_uid, &target)
    {
        return Err(invalid("cleanup differs from exact candidate"));
    }
    for resource in [
        &cleanup.resources.pod,
        &cleanup.resources.pvc,
        &cleanup.resources.endpoint,
    ] {
        if resource.name().is_empty()
            || matches!(resource, CleanupResourceIdentity::Present { uid, .. } if uid.is_empty())
        {
            return Err(invalid(
                "cleanup requires exact identity or positive absence",
            ));
        }
    }
    Ok(())
}

pub fn validate_scale_up_receipt(receipt: &ScaleUpReceipt) -> Result {
    validate_scale_up(&receipt.intent)?;
    validate_witnesses(
        &receipt.intent,
        &receipt.intent.current_configuration,
        &receipt.current_only_write_quorum,
        receipt.intent.current_policy.write_quorum,
        false,
        true,
    )
}

pub fn validate_scale_up_configuration(command: &EnsureConfiguration) -> Result {
    let evidence = command
        .scale_up_evidence
        .as_ref()
        .ok_or_else(|| invalid("missing scale-up configuration evidence"))?;
    let intent = evidence.intent();
    validate_scale_up(intent)?;
    if command.previous_policy.as_ref() != Some(&intent.previous_policy)
        || command.effective_policy != intent.current_policy
        || command.secondary_removal_evidence.is_some()
        || command.switchover_handoff.is_some()
        || !command.retire_switchover_preparation_ids.is_empty()
    {
        return Err(invalid(
            "configuration command differs from scale-up authority",
        ));
    }
    match &**evidence {
        ScaleUpConfigurationEvidence::Admission { .. } => {
            if command.transition_kind != TransitionKind::ScaleUp
                || command.current_configuration != intent.current_configuration
                || command.current_epoch != intent.current_configuration.epoch
                || command.failover_safe_lsn.is_some()
            {
                return Err(invalid("admission command has the wrong transition kind"));
            }
        }
        ScaleUpConfigurationEvidence::Failover { evidence } => {
            validate_scale_up_failover_evidence(evidence)?;
            validate_scale_up_failover_transition(
                evidence,
                &command.current_configuration,
                &command.effective_policy,
            )?;
            if command.transition_kind != TransitionKind::Failover
                || command.current_epoch != command.current_configuration.epoch
                || command.failover_safe_lsn.is_none_or(|lsn| lsn < 0)
            {
                return Err(invalid("failover command has the wrong transition kind"));
            }
        }
    }
    if command.current_only {
        if command.previous_configuration.is_some()
            || command.previous_epoch.is_some()
            || !command
                .retire_build_ids
                .iter()
                .any(|build_id| build_id == &intent.build_id)
        {
            return Err(invalid(
                "current-only completion must omit PC and retire the scale-up build",
            ));
        }
    } else if command.previous_configuration.as_ref() != Some(&intent.previous_configuration)
        || command.previous_epoch != Some(intent.previous_configuration.epoch)
        || !command.retire_build_ids.is_empty()
    {
        return Err(invalid("PC/CC command differs from scale-up authority"));
    }
    let target = ReplicaIdentity {
        replica_id: command.local_replica_id,
        instance_id: command.expected_instance_id.clone(),
        agent_generation: command.expected_agent_generation.clone(),
    };
    if !intent
        .current_configuration
        .members
        .iter()
        .chain(intent.previous_configuration.members.iter())
        .any(|member| member.identity == target)
    {
        return Err(invalid("command target is outside scale-up authority"));
    }
    let stage = if command.current_only {
        ScaleUpStage::CurrentOnly
    } else {
        ScaleUpStage::PreviousCurrent
    };
    if command.operation_id
        != intent.command_operation_id(stage, &target, &command.current_configuration)
    {
        return Err(invalid(
            "command operation ID differs from scale-up authority",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(id: i64) -> ReplicaIdentity {
        ReplicaIdentity {
            replica_id: ReplicaId::new(id),
            instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
            agent_generation: AgentGeneration::new(format!("gen-{id}")),
        }
    }

    fn member(id: i64, role: ReplicaRole) -> ConfigurationMember {
        ConfigurationMember {
            identity: identity(id),
            role,
        }
    }

    fn intent(size: u32) -> ScaleUpIntent {
        let previous_policy = EffectivePolicy::fixed(size, 30).unwrap();
        let current_policy = EffectivePolicy::fixed(size + 1, 30).unwrap();
        let previous = ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            ReplicaId::new(1),
            (1..=i64::from(size))
                .map(|id| {
                    member(
                        id,
                        if id == 1 {
                            ReplicaRole::Primary
                        } else {
                            ReplicaRole::ActiveSecondary
                        },
                    )
                })
                .collect(),
            previous_policy.write_quorum,
        );
        let target = identity(i64::from(size + 1));
        let mut current_members = previous.members.clone();
        current_members.push(ConfigurationMember {
            identity: target.clone(),
            role: ReplicaRole::ActiveSecondary,
        });
        let current = ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            ReplicaId::new(1),
            current_members,
            current_policy.write_quorum,
        );
        let mut intent = ScaleUpIntent {
            operation_id: OperationId::default(),
            resource_uid: ResourceUid::new("set"),
            spec_generation: 2,
            desired_replicas: size + 1,
            previous_configuration: previous,
            current_configuration: current,
            previous_policy,
            current_policy,
            primary: identity(1),
            target,
            build_id: OperationId::new("build"),
            snapshot_boundary_lsn: 0,
            catch_up_boundary_lsn: 0,
        };
        intent.operation_id = intent.expected_operation_id();
        intent
    }

    fn provisioning(intent: &ScaleUpIntent) -> ProvisioningIntent {
        let mut provisioning = ProvisioningIntent {
            purpose: ProvisioningPurpose::scale_up(ScaleUpProvisioning {
                resource_uid: intent.resource_uid.clone(),
                spec_generation: intent.spec_generation,
                desired_replicas: intent.desired_replicas,
                previous_configuration: intent.previous_configuration.clone(),
                previous_policy: intent.previous_policy.clone(),
                current_policy: intent.current_policy.clone(),
                target_replica_id: intent.target.replica_id,
            }),
            pod_uid: PodUid::new(intent.target.instance_id.as_str()),
            pvc_uid: PvcUid::new("pvc-2"),
            operation_id: OperationId::default(),
        };
        provisioning.operation_id = provisioning.expected_operation_id();
        provisioning
    }

    fn witness(
        intent: &ScaleUpIntent,
        identity: ReplicaIdentity,
        previous_current: bool,
        sequence: u64,
    ) -> ScaleUpWitness {
        let role = intent
            .current_configuration
            .members
            .iter()
            .find(|member| member.identity == identity)
            .unwrap()
            .role;
        let retained_operation_id = intent.command_operation_id(
            if previous_current {
                ScaleUpStage::PreviousCurrent
            } else {
                ScaleUpStage::CurrentOnly
            },
            &identity,
            &intent.current_configuration,
        );
        let write_status = if !previous_current && identity == intent.primary {
            AccessStatus::Granted
        } else {
            AccessStatus::ReconfigurationPending
        };
        ScaleUpWitness {
            resource_uid: intent.resource_uid.clone(),
            identity,
            role,
            process_session_id: ProcessSessionId::new(format!("session-{sequence}")),
            report_sequence: sequence,
            epoch: intent.current_configuration.epoch,
            previous_configuration_id: previous_current
                .then(|| intent.previous_configuration.configuration_id.clone()),
            current_configuration_id: intent.current_configuration.configuration_id.clone(),
            verified_replication_lsn: intent.catch_up_boundary_lsn,
            write_status,
            pending_operation_id: None,
            retained_operation_id: Some(retained_operation_id),
        }
    }

    #[test]
    fn validates_zero_boundary_one_member_increase() {
        assert_eq!(validate_scale_up(&intent(1)), Ok(()));
    }

    #[test]
    fn rejects_membership_policy_boundary_and_operation_corruption() {
        for mutation in 0..7 {
            let mut invalid = intent(2);
            match mutation {
                0 => invalid.target.replica_id = ReplicaId::new(4),
                1 => {
                    let mut members = invalid.current_configuration.members.clone();
                    for member in &mut members {
                        member.role = if member.identity.replica_id == ReplicaId::new(2) {
                            ReplicaRole::Primary
                        } else {
                            ReplicaRole::ActiveSecondary
                        };
                    }
                    invalid.current_configuration = ConfigurationDescriptor::new(
                        invalid.current_configuration.epoch,
                        ReplicaId::new(2),
                        members,
                        invalid.current_policy.write_quorum,
                    );
                }
                2 => invalid.current_policy = EffectivePolicy::fixed(4, 30).unwrap(),
                3 => invalid.catch_up_boundary_lsn = -1,
                4 => {
                    invalid.snapshot_boundary_lsn = 2;
                    invalid.catch_up_boundary_lsn = 1;
                }
                5 => {
                    let mut members = invalid.current_configuration.members.clone();
                    members[0].identity.agent_generation = AgentGeneration::new("different");
                    invalid.current_configuration = ConfigurationDescriptor::new(
                        invalid.current_configuration.epoch,
                        invalid.current_configuration.primary_id,
                        members,
                        invalid.current_policy.write_quorum,
                    );
                }
                _ => {
                    invalid.operation_id = OperationId::new("other");
                    assert!(validate_scale_up(&invalid).is_err());
                    continue;
                }
            }
            invalid.operation_id = invalid.expected_operation_id();
            assert!(validate_scale_up(&invalid).is_err(), "mutation {mutation}");
        }
    }

    #[test]
    fn canonical_scale_up_rejects_empty_candidate_incarnation() {
        for mutation in 0..2 {
            let mut invalid = intent(2);
            if mutation == 0 {
                invalid.target.instance_id = ReplicaInstanceId::default();
            } else {
                invalid.target.agent_generation = AgentGeneration::default();
            }
            let target = invalid
                .current_configuration
                .members
                .iter_mut()
                .find(|member| member.identity.replica_id == invalid.target.replica_id)
                .unwrap();
            target.identity = invalid.target.clone();
            invalid.current_configuration = ConfigurationDescriptor::new(
                invalid.current_configuration.epoch,
                invalid.current_configuration.primary_id,
                invalid.current_configuration.members,
                invalid.current_policy.write_quorum,
            );
            invalid.operation_id = invalid.expected_operation_id();
            assert!(validate_scale_up(&invalid).is_err(), "mutation {mutation}");
        }
    }

    #[test]
    fn provisioning_is_tagged_exact_and_overflow_safe() {
        let intent = intent(1);
        let provisioning = provisioning(&intent);
        assert_eq!(validate_scale_up_provisioning(&provisioning), Ok(()));
        let build_id = provisioning
            .scale_up_build_id(&intent.resource_uid)
            .unwrap();
        let mut different_candidate = provisioning.clone();
        different_candidate.pod_uid = PodUid::new("other-pod");
        different_candidate.operation_id = different_candidate.expected_operation_id();
        assert_ne!(
            different_candidate
                .scale_up_build_id(&intent.resource_uid)
                .unwrap(),
            build_id
        );

        let mut malformed = provisioning.clone();
        malformed.purpose.replaces = Some(identity(1));
        assert!(validate_scale_up_provisioning(&malformed).is_err());

        let mut overflow = provisioning;
        let scale_up = overflow.purpose.scale_up.as_mut().unwrap();
        scale_up.previous_configuration = ConfigurationDescriptor::new(
            Epoch::new(0, i64::MAX),
            ReplicaId::new(1),
            vec![member(1, ReplicaRole::Primary)],
            scale_up.previous_policy.write_quorum,
        );
        overflow.operation_id = overflow.expected_operation_id();
        assert!(validate_scale_up_provisioning(&overflow).is_err());
    }

    #[test]
    fn cleanup_is_bound_to_fresh_candidate_resources() {
        let intent = intent(1);
        let provisioning = provisioning(&intent);
        let target = provisioning.target_identity(&intent.resource_uid);
        let cleanup = ScaleUpCleanup {
            provisioning,
            target: target.clone(),
            resources: ReplicaCleanupIdentity {
                pod: CleanupResourceIdentity::Present {
                    name: "db-1".into(),
                    uid: target.instance_id.to_string(),
                },
                pvc: CleanupResourceIdentity::Present {
                    name: "db-1-data".into(),
                    uid: "pvc-2".into(),
                },
                endpoint: CleanupResourceIdentity::Present {
                    name: derive_replica_endpoint_name(&intent.resource_uid, &target),
                    uid: "service-2".into(),
                },
            },
        };
        assert_eq!(validate_scale_up_cleanup(&cleanup), Ok(()));
        let mut replaced = cleanup;
        replaced.target.instance_id = ReplicaInstanceId::new("replacement");
        assert!(validate_scale_up_cleanup(&replaced).is_err());
    }

    #[test]
    fn failover_evidence_uses_independent_read_quorums() {
        let intent = intent(2);
        let previous = intent.previous_configuration.members[0].identity.clone();
        let current_a = intent.current_configuration.members[0].identity.clone();
        let current_b = intent.current_configuration.members[1].identity.clone();
        let evidence = ScaleUpFailoverEvidence {
            previous_read_quorum: vec![witness(&intent, previous, true, 1)],
            current_read_quorum: vec![
                witness(&intent, current_a, true, 2),
                witness(&intent, current_b, true, 3),
            ],
            intent,
        };
        assert_eq!(validate_scale_up_failover_evidence(&evidence), Ok(()));
        let mut insufficient = evidence;
        insufficient.current_read_quorum.pop();
        assert!(validate_scale_up_failover_evidence(&insufficient).is_err());
    }

    #[test]
    fn failover_commands_bind_the_superseding_configuration() {
        let intent = intent(2);
        let previous_witness = witness(
            &intent,
            intent.previous_configuration.members[1].identity.clone(),
            true,
            1,
        );
        let current_witnesses = vec![
            witness(
                &intent,
                intent.current_configuration.members[1].identity.clone(),
                true,
                2,
            ),
            witness(
                &intent,
                intent.current_configuration.members[2].identity.clone(),
                true,
                3,
            ),
        ];
        let evidence = ScaleUpFailoverEvidence {
            intent: intent.clone(),
            previous_read_quorum: vec![previous_witness],
            current_read_quorum: current_witnesses,
        };
        let mut members = intent.current_configuration.members.clone();
        for member in &mut members {
            member.role = if member.identity.replica_id == ReplicaId::new(2) {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            };
        }
        let failover = ConfigurationDescriptor::new(
            Epoch::new(0, 3),
            ReplicaId::new(2),
            members,
            intent.current_policy.write_quorum,
        );
        let target = failover
            .members
            .iter()
            .find(|member| member.identity.replica_id == ReplicaId::new(2))
            .unwrap()
            .identity
            .clone();
        let admission_id = intent.command_operation_id(
            ScaleUpStage::PreviousCurrent,
            &target,
            &intent.current_configuration,
        );
        let failover_id =
            intent.command_operation_id(ScaleUpStage::PreviousCurrent, &target, &failover);
        assert_ne!(admission_id, failover_id);
        let command = EnsureConfiguration {
            operation_id: failover_id,
            previous_configuration: Some(intent.previous_configuration.clone()),
            current_configuration: failover.clone(),
            previous_epoch: Some(intent.previous_configuration.epoch),
            current_epoch: failover.epoch,
            effective_policy: intent.current_policy.clone(),
            previous_policy: Some(intent.previous_policy.clone()),
            secondary_removal_evidence: None,
            scale_up_evidence: Some(Box::new(ScaleUpConfigurationEvidence::Failover {
                evidence,
            })),
            local_replica_id: target.replica_id,
            expected_instance_id: target.instance_id,
            expected_agent_generation: target.agent_generation,
            transition_kind: TransitionKind::Failover,
            failover_safe_lsn: Some(0),
            primary_write_status: AccessStatus::Granted,
            current_only: false,
            retire_build_ids: Vec::new(),
            switchover_handoff: None,
            retire_switchover_preparation_ids: Vec::new(),
        };
        assert_eq!(validate_scale_up_configuration(&command), Ok(()));
    }

    #[test]
    fn representative_scale_up_status_evidence_remains_bounded() {
        let intent = intent(3);
        let provisioning = provisioning(&intent);
        let receipt = ScaleUpReceipt {
            current_only_write_quorum: intent
                .current_configuration
                .members
                .iter()
                .take(intent.current_policy.write_quorum as usize)
                .enumerate()
                .map(|(index, member)| {
                    witness(&intent, member.identity.clone(), false, index as u64 + 1)
                })
                .collect(),
            intent,
        };
        validate_scale_up_receipt(&receipt).unwrap();
        let target = provisioning.target_identity(&receipt.intent.resource_uid);
        let cleanup = ScaleUpCleanup {
            provisioning: provisioning.clone(),
            target: target.clone(),
            resources: ReplicaCleanupIdentity {
                pod: CleanupResourceIdentity::Present {
                    name: "db-3".into(),
                    uid: target.instance_id.to_string(),
                },
                pvc: CleanupResourceIdentity::Present {
                    name: "db-3-data".into(),
                    uid: provisioning.pvc_uid.to_string(),
                },
                endpoint: CleanupResourceIdentity::Present {
                    name: derive_replica_endpoint_name(&receipt.intent.resource_uid, &target),
                    uid: "service-4".into(),
                },
            },
        };
        let statuses = [
            (
                "provisioning",
                AcceptedStatus {
                    initialized: true,
                    effective_policy: Some(receipt.intent.previous_policy.clone()),
                    topology: Some(AcceptedTopology {
                        configuration: receipt.intent.previous_configuration.clone(),
                    }),
                    provisioning: Some(provisioning),
                    ..Default::default()
                },
            ),
            (
                "cleanup",
                AcceptedStatus {
                    initialized: true,
                    effective_policy: Some(receipt.intent.previous_policy.clone()),
                    topology: Some(AcceptedTopology {
                        configuration: receipt.intent.previous_configuration.clone(),
                    }),
                    scale_up_cleanup: Some(Box::new(cleanup)),
                    ..Default::default()
                },
            ),
            (
                "receipt",
                AcceptedStatus {
                    initialized: true,
                    effective_policy: Some(receipt.intent.current_policy.clone()),
                    topology: Some(AcceptedTopology {
                        configuration: receipt.intent.current_configuration.clone(),
                    }),
                    last_scale_up: Some(Box::new(receipt)),
                    ..Default::default()
                },
            ),
        ];
        for (label, status) in statuses {
            crate::validation::validate_status(&status).unwrap();
            let bytes = serde_json::to_vec(&status).unwrap();
            assert!(
                bytes.len() < 32_768,
                "{label} scale-up status grew to {} bytes",
                bytes.len()
            );
        }
    }

    #[test]
    fn receipt_witnesses_are_bound_to_the_exact_build_attempt() {
        let intent = intent(2);
        let mut receipt = ScaleUpReceipt {
            current_only_write_quorum: intent
                .current_configuration
                .members
                .iter()
                .take(intent.current_policy.write_quorum as usize)
                .enumerate()
                .map(|(index, member)| {
                    witness(&intent, member.identity.clone(), false, index as u64 + 1)
                })
                .collect(),
            intent,
        };
        validate_scale_up_receipt(&receipt).unwrap();
        receipt.intent.build_id = OperationId::new("other-build");
        receipt.intent.operation_id = receipt.intent.expected_operation_id();
        assert!(validate_scale_up_receipt(&receipt).is_err());
    }

    #[test]
    fn active_transition_binds_accepted_and_expanded_authority() {
        let intent = intent(2);
        let status = AcceptedStatus {
            initialized: true,
            effective_policy: Some(intent.previous_policy.clone()),
            topology: Some(AcceptedTopology {
                configuration: intent.previous_configuration.clone(),
            }),
            transition: Some(TransitionIntent {
                transition_id: intent
                    .transition_id(TransitionKind::ScaleUp, &intent.current_configuration),
                kind: TransitionKind::ScaleUp,
                spec_generation: intent.spec_generation,
                effective_policy: intent.current_policy.clone(),
                previous_configuration_id: Some(
                    intent.previous_configuration.configuration_id.clone(),
                ),
                current_configuration: intent.current_configuration.clone(),
                election_lsn: None,
                build_id: Some(intent.build_id.clone()),
                repair: None,
                switchover: None,
                secondary_scale_down: None,
                secondary_removal_evidence: None,
                scale_up: Some(Box::new(intent.clone())),
                scale_up_failover: None,
            }),
            ..Default::default()
        };
        assert_eq!(crate::validation::validate_status(&status), Ok(()));

        let mut invalid = status;
        invalid
            .transition
            .as_mut()
            .unwrap()
            .current_configuration
            .primary_id = ReplicaId::new(2);
        assert!(crate::validation::validate_status(&invalid).is_err());
    }

    #[test]
    fn pending_candidate_cleanup_allows_ordinary_primary_failover() {
        let intent = intent(2);
        let provisioning = provisioning(&intent);
        let target = provisioning.target_identity(&intent.resource_uid);
        let cleanup = ScaleUpCleanup {
            provisioning,
            target: target.clone(),
            resources: ReplicaCleanupIdentity {
                pod: CleanupResourceIdentity::Present {
                    name: "db-2".into(),
                    uid: target.instance_id.to_string(),
                },
                pvc: CleanupResourceIdentity::Present {
                    name: "db-2-data".into(),
                    uid: "pvc-2".into(),
                },
                endpoint: CleanupResourceIdentity::Present {
                    name: derive_replica_endpoint_name(&intent.resource_uid, &target),
                    uid: "service-2".into(),
                },
            },
        };
        let mut members = intent.previous_configuration.members.clone();
        for member in &mut members {
            member.role = if member.identity.replica_id == ReplicaId::new(2) {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            };
        }
        let failover = ConfigurationDescriptor::new(
            Epoch::new(0, 2),
            ReplicaId::new(2),
            members,
            intent.previous_policy.write_quorum,
        );
        let status = AcceptedStatus {
            initialized: true,
            effective_policy: Some(intent.previous_policy.clone()),
            topology: Some(AcceptedTopology {
                configuration: intent.previous_configuration.clone(),
            }),
            transition: Some(TransitionIntent {
                transition_id: derive_transition_id(
                    &intent.resource_uid,
                    TransitionKind::Failover,
                    &failover.configuration_id,
                ),
                kind: TransitionKind::Failover,
                spec_generation: intent.spec_generation,
                effective_policy: intent.previous_policy.clone(),
                previous_configuration_id: Some(
                    intent.previous_configuration.configuration_id.clone(),
                ),
                current_configuration: failover,
                election_lsn: Some(0),
                build_id: None,
                repair: None,
                switchover: None,
                secondary_scale_down: None,
                secondary_removal_evidence: None,
                scale_up: None,
                scale_up_failover: None,
            }),
            scale_up_cleanup: Some(Box::new(cleanup)),
            ..Default::default()
        };
        assert_eq!(crate::validation::validate_status(&status), Ok(()));
        let mut accepted = status;
        accepted.topology = Some(AcceptedTopology {
            configuration: accepted
                .transition
                .as_ref()
                .unwrap()
                .current_configuration
                .clone(),
        });
        accepted.transition = None;
        assert_eq!(crate::validation::validate_status(&accepted), Ok(()));
    }

    #[test]
    fn pending_candidate_cleanup_rejects_failover_that_admits_the_candidate() {
        let mut intent = intent(1);
        let provisioning = provisioning(&intent);
        let candidate = provisioning.target_identity(&intent.resource_uid);
        intent.target = candidate.clone();
        let mut expanded_members = intent.current_configuration.members.clone();
        expanded_members
            .iter_mut()
            .find(|member| member.identity.replica_id == candidate.replica_id)
            .unwrap()
            .identity = candidate.clone();
        intent.current_configuration = ConfigurationDescriptor::new(
            intent.current_configuration.epoch,
            intent.current_configuration.primary_id,
            expanded_members,
            intent.current_policy.write_quorum,
        );
        intent.operation_id = intent.expected_operation_id();
        let cleanup = ScaleUpCleanup {
            provisioning,
            target: candidate.clone(),
            resources: ReplicaCleanupIdentity {
                pod: CleanupResourceIdentity::Present {
                    name: "db-1".into(),
                    uid: candidate.instance_id.to_string(),
                },
                pvc: CleanupResourceIdentity::Present {
                    name: "db-1-data".into(),
                    uid: "pvc-2".into(),
                },
                endpoint: CleanupResourceIdentity::Present {
                    name: derive_replica_endpoint_name(&intent.resource_uid, &candidate),
                    uid: "service-2".into(),
                },
            },
        };
        let evidence = ScaleUpFailoverEvidence {
            previous_read_quorum: vec![witness(
                &intent,
                intent.previous_configuration.members[0].identity.clone(),
                true,
                1,
            )],
            current_read_quorum: vec![witness(&intent, candidate.clone(), true, 2)],
            intent: intent.clone(),
        };
        let mut members = intent.current_configuration.members.clone();
        for member in &mut members {
            member.role = if member.identity == candidate {
                ReplicaRole::Primary
            } else {
                ReplicaRole::ActiveSecondary
            };
        }
        let failover = ConfigurationDescriptor::new(
            Epoch::new(0, 3),
            candidate.replica_id,
            members,
            intent.current_policy.write_quorum,
        );
        let mut status = AcceptedStatus {
            initialized: true,
            effective_policy: Some(intent.previous_policy.clone()),
            topology: Some(AcceptedTopology {
                configuration: intent.previous_configuration.clone(),
            }),
            transition: Some(TransitionIntent {
                transition_id: intent.transition_id(TransitionKind::Failover, &failover),
                kind: TransitionKind::Failover,
                spec_generation: intent.spec_generation,
                effective_policy: intent.current_policy.clone(),
                previous_configuration_id: Some(
                    intent.previous_configuration.configuration_id.clone(),
                ),
                current_configuration: failover,
                election_lsn: Some(0),
                build_id: Some(intent.build_id.clone()),
                repair: None,
                switchover: None,
                secondary_scale_down: None,
                secondary_removal_evidence: None,
                scale_up: None,
                scale_up_failover: Some(Box::new(evidence)),
            }),
            ..Default::default()
        };
        assert_eq!(crate::validation::validate_status(&status), Ok(()));
        status.scale_up_cleanup = Some(Box::new(cleanup));
        assert!(crate::validation::validate_status(&status).is_err());
    }

    #[test]
    fn typed_report_evidence_allows_only_the_exact_expansion() {
        let intent = intent(2);
        let primary = intent.primary.clone();
        let report = crate::observation::AgentReport {
            protocol_version: crate::PROTOCOL_VERSION,
            resource_uid: intent.resource_uid.clone(),
            identity: primary.clone(),
            process_session_id: ProcessSessionId::new("session"),
            report_sequence: 1,
            role: ReplicaRole::Primary,
            read_status: AccessStatus::Granted,
            write_status: AccessStatus::Granted,
            healthy: true,
            epoch: intent.current_configuration.epoch,
            previous_configuration: Some(intent.previous_configuration.clone()),
            current_configuration: Some(intent.current_configuration.clone()),
            current_progress: 0,
            verified_replication_lsn: Some(0),
            committed_lsn: 0,
            retained_operation_id: Some(intent.command_operation_id(
                ScaleUpStage::PreviousCurrent,
                &primary,
                &intent.current_configuration,
            )),
            scale_up_intent: Some(Box::new(intent.clone())),
            ..Default::default()
        };
        assert_eq!(crate::validation::validate_report_internal(&report), Ok(()));

        let mut missing = report.clone();
        missing.scale_up_intent = None;
        assert!(crate::validation::validate_report_internal(&missing).is_err());

        let mut other = report;
        other.resource_uid = ResourceUid::new("other");
        assert!(crate::validation::validate_report_internal(&other).is_err());
    }
}
