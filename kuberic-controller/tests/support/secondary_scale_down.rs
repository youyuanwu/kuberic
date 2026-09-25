use super::*;
use kuberic_protocol::command::{KubernetesChange, ScaleDownResource};
use kuberic_protocol::observation::ExactResourceObservation;
use kuberic_protocol::types::*;

fn enabled() -> EvaluationConfig {
    EvaluationConfig {
        enable_secondary_scale_down: true,
        ..config()
    }
}

fn fixture(count: u32, desired: u32) -> RawObservation {
    let mut raw = raw(desired);
    raw.set.spec.failover_delay_seconds = 10;
    raw.set.metadata.generation = Some(2);
    let policy = EffectivePolicy::fixed(count, 10).unwrap();
    let members = (1..=count)
        .map(|id| {
            let replica_id = ReplicaId::new(i64::from(id));
            let pod_uid = format!("pod-uid-{id}");
            let pvc_uid = format!("pvc-uid-{id}");
            let identity = ReplicaIdentity {
                replica_id,
                instance_id: ReplicaInstanceId::new(&pod_uid),
                agent_generation: derive_agent_generation(&derive_initialization_id(
                    &ResourceUid::new(UID),
                    replica_id,
                    &PodUid::new(&pod_uid),
                    &PvcUid::new(&pvc_uid),
                )),
            };
            let mut labels = labels(replica_id);
            labels.insert(INSTANCE_LABEL.into(), pod_uid.clone());
            raw.pods.push(Pod {
                metadata: kube::core::ObjectMeta {
                    name: Some(format!("db-{id}")),
                    uid: Some(pod_uid.clone()),
                    resource_version: Some("10".into()),
                    labels: Some(labels.clone()),
                    ..Default::default()
                },
                spec: Some(PodSpec {
                    containers: vec![k8s_openapi::api::core::v1::Container {
                        name: "application".into(),
                        image: Some(raw.set.spec.image.clone()),
                        ..Default::default()
                    }],
                    volumes: Some(vec![Volume {
                        name: "data".into(),
                        persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                            claim_name: format!("db-{id}-data"),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                status: Some(PodStatus {
                    pod_ip: Some("127.0.0.1".into()),
                    conditions: Some(vec![PodCondition {
                        type_: "Ready".into(),
                        status: "True".into(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
            });
            raw.pvcs.push(PersistentVolumeClaim {
                metadata: kube::core::ObjectMeta {
                    name: Some(format!("db-{id}-data")),
                    uid: Some(pvc_uid),
                    resource_version: Some("11".into()),
                    labels: Some(labels.clone()),
                    ..Default::default()
                },
                ..Default::default()
            });
            raw.services.push(Service {
                metadata: kube::core::ObjectMeta {
                    name: Some(derive_replica_endpoint_name(
                        &ResourceUid::new(UID),
                        &identity,
                    )),
                    uid: Some(format!("endpoint-{id}")),
                    resource_version: Some("12".into()),
                    labels: Some(labels),
                    ..Default::default()
                },
                spec: Some(ServiceSpec {
                    selector: Some(BTreeMap::from([(INSTANCE_LABEL.into(), pod_uid)])),
                    ports: Some(vec![
                        ServicePort {
                            name: Some("control".into()),
                            port: 50051,
                            ..Default::default()
                        },
                        ServicePort {
                            name: Some("replication".into()),
                            port: 50052,
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            });
            ConfigurationMember {
                identity,
                role: if id == 1 {
                    ReplicaRole::Primary
                } else {
                    ReplicaRole::ActiveSecondary
                },
            }
        })
        .collect();
    let configuration = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        ReplicaId::new(1),
        members,
        policy.write_quorum,
    );
    for member in &configuration.members {
        let mut report = initialized_report(member.identity.clone(), configuration.clone());
        report.replica_id = member.identity.replica_id.value();
        report.role = if member.role == ReplicaRole::Primary {
            proto::ReplicaRole::Primary as i32
        } else {
            proto::ReplicaRole::ActiveSecondary as i32
        };
        report.write_status = if member.role == ReplicaRole::Primary {
            proto::AccessStatus::Granted as i32
        } else {
            proto::AccessStatus::NotPrimary as i32
        };
        report.verified_replication_lsn = Some(5);
        report.pod_uid = member.identity.instance_id.to_string();
        report.pvc_uid = format!("pvc-uid-{}", member.identity.replica_id);
        report.process_session_id = format!("session-{}", member.identity.replica_id);
        raw.agents.insert(
            ReplicaObservationKey::new(
                member.identity.replica_id,
                member.identity.instance_id.clone(),
            ),
            RawAgentObservation::Report(Box::new(report)),
        );
    }
    raw.set.status = Some(KubericSetStatus {
        authority: AcceptedStatus {
            initialized: true,
            observed_generation: 1,
            effective_policy: Some(policy),
            topology: Some(AcceptedTopology { configuration }),
            ..Default::default()
        },
    });
    raw.services.push(write_service("pod-uid-1"));
    raw
}

async fn apply_command(api: &InMemoryClusterApi, command: &ProtocolCommand) {
    let mut raw = api.observation().await;
    let target = match command {
        ProtocolCommand::PrepareSecondaryRemoval(c) => c.intent.primary.clone(),
        ProtocolCommand::EnsureConfiguration(c) => ReplicaIdentity {
            replica_id: c.local_replica_id,
            instance_id: c.expected_instance_id.clone(),
            agent_generation: c.expected_agent_generation.clone(),
        },
        ProtocolCommand::AcceptSecondaryRemovalCommit(c) => c.target.clone(),
        ProtocolCommand::RetireReplica(c) => c.committed.evidence.preparation.intent.target.clone(),
        other => panic!("unexpected removal command {other:?}"),
    };
    let RawAgentObservation::Report(report) = raw
        .agents
        .get_mut(&ReplicaObservationKey::new(
            target.replica_id,
            target.instance_id.clone(),
        ))
        .unwrap()
    else {
        panic!("exact target report")
    };
    match command {
        ProtocolCommand::PrepareSecondaryRemoval(c) => {
            report.read_status = proto::AccessStatus::ReconfigurationPending as i32;
            report.write_status = proto::AccessStatus::ReconfigurationPending as i32;
            report.prepared_secondary_removal = Some(
                SecondaryRemovalPreparation {
                    intent: c.intent.clone(),
                    operation_id: c.operation_id.clone(),
                    process_session_id: ProcessSessionId::new(&report.process_session_id),
                    report_sequence: report.report_sequence,
                    boundary_lsn: 5,
                }
                .into(),
            );
            report.retained_operation_id = c.operation_id.to_string();
        }
        ProtocolCommand::EnsureConfiguration(c) => {
            report.epoch = Some(c.current_epoch.into());
            report.previous_configuration = c.previous_configuration.clone().map(Into::into);
            report.current_configuration = Some(c.current_configuration.clone().into());
            if let Some(evidence) = &c.secondary_removal_evidence {
                report.secondary_removal_evidence = Some(evidence.clone().into());
            }
            report.role = if c.current_configuration.primary_id == c.local_replica_id {
                proto::ReplicaRole::Primary as i32
            } else {
                proto::ReplicaRole::ActiveSecondary as i32
            };
            report.write_status = if c.primary_write_status == AccessStatus::Granted {
                proto::AccessStatus::Granted as i32
            } else if c.local_replica_id == c.current_configuration.primary_id {
                proto::AccessStatus::ReconfigurationPending as i32
            } else {
                proto::AccessStatus::NotPrimary as i32
            };
            report.read_status = report.write_status;
            report.retained_operation_id = c.operation_id.to_string();
            report.catch_up_complete = true;
            report.catch_up_boundary = c
                .secondary_removal_evidence
                .as_ref()
                .map(|e| e.preparation.boundary_lsn);
            report.current_configuration_quorum_progress = 5;
            report.pending_operation_id.clear();
        }
        ProtocolCommand::AcceptSecondaryRemovalCommit(c) => {
            report.accepted_secondary_removal = Some(c.committed.clone().into());
            report.prepared_secondary_removal = None;
        }
        ProtocolCommand::RetireReplica(c) => {
            let intent = &c.committed.evidence.preparation.intent;
            report.retired_replica = Some(
                ReplicaRetirementReport {
                    intent: intent.clone(),
                    operation_id: c.operation_id.clone(),
                    process_session_id: ProcessSessionId::new(&report.process_session_id),
                    report_sequence: report.report_sequence,
                    epoch: intent.current_configuration.epoch,
                    role: ReplicaRole::None,
                    read_status: AccessStatus::NotPrimary,
                    write_status: AccessStatus::NotPrimary,
                    application_closed: true,
                    peers_fenced: true,
                }
                .into(),
            );
            report.role = proto::ReplicaRole::None as i32;
            report.epoch = Some(intent.current_configuration.epoch.into());
            report.read_status = proto::AccessStatus::NotPrimary as i32;
            report.write_status = proto::AccessStatus::NotPrimary as i32;
            report.previous_configuration = None;
            report.current_configuration = None;
            report.verified_replication_lsn = None;
            report.secondary_removal_evidence = None;
            report.accepted_secondary_removal = None;
            report.prepared_secondary_removal = None;
            report.retained_operation_id = c.operation_id.to_string();
            report.pending_operation_id.clear();
        }
        _ => unreachable!(),
    }
    api.set_observation(raw).await;
}

async fn tick(api: &Arc<InMemoryClusterApi>) -> (ReconcileKind, Vec<EffectRecord>) {
    let mut restored = api.observation().await;
    restored.set = serde_json::from_value(serde_json::to_value(&restored.set).unwrap()).unwrap();
    api.set_observation(restored).await;
    refresh_switchover_reports(api).await;
    let before = api.effects().await.len();
    let observed = api.observation_count().await;
    let action = Reconciler::new(api.clone(), enabled())
        .reconcile("tests", "db")
        .await
        .unwrap();
    assert_eq!(api.observation_count().await, observed + 1);
    let effects = api.effects().await[before..].to_vec();
    assert!(
        effects.len() <= 1,
        "one effect per fresh observation: {effects:?}; {:?}",
        api.observation()
            .await
            .set
            .status
            .unwrap()
            .authority
            .conditions
    );
    for effect in &effects {
        if let EffectRecord::Execute(command) = effect {
            apply_command(api, command).await;
        }
    }
    (action.kind, effects)
}

async fn next_plan(api: &InMemoryClusterApi) -> (RawObservation, Plan) {
    let raw = api.observe("tests", "db").await.unwrap();
    let plan = evaluate(
        &normalize(raw.clone(), BTreeMap::new()).unwrap(),
        &enabled(),
    );
    (raw, plan)
}

async fn finish(api: &Arc<InMemoryClusterApi>) {
    for _ in 0..250 {
        if tick(api).await.0 == ReconcileKind::Stable {
            return;
        }
    }
    panic!("did not converge: {:?}", next_plan(api).await.1);
}

async fn at_delete(
    api: &Arc<InMemoryClusterApi>,
    resource: ScaleDownResource,
) -> (RawObservation, KubernetesChange) {
    for _ in 0..100 {
        let (raw, plan) = next_plan(api).await;
        if let Plan::Apply { changes } = plan
            && let Some(change @ KubernetesChange::DeleteScaleDownResource { resource: kind, .. }) =
                changes.first()
            && *kind == resource
        {
            return (raw, change.clone());
        }
        tick(api).await;
    }
    panic!("no {resource:?} deletion: {:?}", next_plan(api).await.1)
}

fn metadata<'a>(
    raw: &'a mut RawObservation,
    resource: ScaleDownResource,
    name: &str,
) -> &'a mut kube::core::ObjectMeta {
    match resource {
        ScaleDownResource::Pod => {
            &mut raw
                .pods
                .iter_mut()
                .find(|o| o.name_any() == name)
                .unwrap()
                .metadata
        }
        ScaleDownResource::Pvc => {
            &mut raw
                .pvcs
                .iter_mut()
                .find(|o| o.name_any() == name)
                .unwrap()
                .metadata
        }
        ScaleDownResource::Endpoint => {
            &mut raw
                .services
                .iter_mut()
                .find(|o| o.name_any() == name)
                .unwrap()
                .metadata
        }
    }
}

#[tokio::test]
async fn healthy_reductions_are_sequential_exact_and_quiescent() {
    for (count, desired) in [(3, 2), (2, 1), (3, 1), (5, 2)] {
        let original = fixture(count, desired);
        let retained_pods = original.pods[..desired as usize].to_vec();
        let retained_pvcs = original.pvcs[..desired as usize].to_vec();
        let api = Arc::new(InMemoryClusterApi::new(original));
        finish(&api).await;
        let completed = api.observation().await;
        let status = completed.set.status.unwrap().authority;
        let topology = status.topology.unwrap().configuration;
        assert_eq!(topology.members.len(), desired as usize);
        assert_eq!(topology.primary_id, ReplicaId::new(1));
        assert_eq!(
            topology.epoch,
            Epoch::new(0, 1 + i64::from(count - desired))
        );
        assert!(status.transition.is_none() && status.secondary_scale_down_cleanup.is_none());
        assert!(status.last_secondary_removal.is_some());
        assert_eq!(completed.pods, retained_pods);
        assert_eq!(completed.pvcs, retained_pvcs);
        let effects = api.effects().await;
        let prepared = effects
            .iter()
            .filter_map(|e| {
                if let EffectRecord::Execute(ProtocolCommand::PrepareSecondaryRemoval(c)) = e {
                    Some(c.intent.target.replica_id.value())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        assert_eq!(
            prepared,
            ((desired + 1)..=count)
                .rev()
                .map(i64::from)
                .collect::<Vec<_>>()
        );
        let cleanup = effects
            .iter()
            .filter_map(|e| match e {
                EffectRecord::DeleteScaleDownResource { resource, .. } => {
                    Some(format!("{resource:?}"))
                }
                EffectRecord::Execute(ProtocolCommand::RetireReplica(_)) => Some("retire".into()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            cleanup,
            ["Endpoint", "retire", "Pod", "Pvc"].repeat((count - desired) as usize)
        );
        for _ in 0..3 {
            assert_eq!(tick(&api).await.0, ReconcileKind::Stable);
        }
        assert_eq!(api.effects().await, effects);
    }
}

#[tokio::test]
async fn commit_status_conflict_blocks_all_cleanup_then_heals() {
    let api = Arc::new(InMemoryClusterApi::new(fixture(3, 2)));
    for _ in 0..80 {
        let (_, plan) = next_plan(&api).await;
        if matches!(plan, Plan::Apply { ref changes } if matches!(changes.first(), Some(KubernetesChange::PersistStatus { status }) if status.secondary_scale_down_cleanup.is_some()))
        {
            let before = api.observation().await;
            api.conflict_next_status().await;
            let (kind, effects) = tick(&api).await;
            assert_eq!(kind, ReconcileKind::ObservationStale);
            assert!(effects.is_empty());
            assert_eq!(before.set.status, api.observation().await.set.status);
            assert_eq!(api.observation().await.pods.len(), 3);
            finish(&api).await;
            return;
        }
        tick(&api).await;
    }
    panic!("commit boundary not reached")
}

#[tokio::test]
async fn exact_cleanup_survives_restart_lost_delete_reply_and_finalizers() {
    for resource in [
        ScaleDownResource::Endpoint,
        ScaleDownResource::Pod,
        ScaleDownResource::Pvc,
    ] {
        let baseline = Arc::new(InMemoryClusterApi::new(fixture(3, 2)));
        let (_, change) = at_delete(&baseline, resource).await;
        let KubernetesChange::DeleteScaleDownResource { name, .. } = change else {
            unreachable!()
        };
        let mut stored = baseline.observation().await;
        metadata(&mut stored, resource, &name).finalizers = Some(vec!["test/finalizer".into()]);
        let api = Arc::new(InMemoryClusterApi::new(stored));
        for _ in 0..3 {
            let (_, effects) = tick(&api).await;
            assert!(
                matches!(effects.as_slice(), [EffectRecord::DeleteScaleDownResource { resource: r, .. }] if *r == resource)
            );
            assert!(
                api.observation()
                    .await
                    .set
                    .status
                    .unwrap()
                    .authority
                    .secondary_scale_down_cleanup
                    .is_some()
            );
            assert_eq!(api.observation().await.pvcs.len(), 3);
        }
        let mut stored = api.observation().await;
        metadata(&mut stored, resource, &name).finalizers = None;
        api.set_observation(stored).await;
        api.lose_next_delete_reply().await;
        assert_eq!(tick(&api).await.0, ReconcileKind::ObservationStale);
        let restarted = Arc::new(InMemoryClusterApi::new(api.observation().await));
        finish(&restarted).await;
        assert!(!restarted.effects().await.iter().any(|e| matches!(e, EffectRecord::DeleteScaleDownResource { resource: r, .. } if *r == resource)));
    }
}

#[tokio::test]
async fn published_removal_commit_survives_retained_peer_session_restart() {
    for id in [1, 2] {
        let api = Arc::new(InMemoryClusterApi::new(fixture(3, 2)));
        let frozen = loop {
            let (raw, plan) = next_plan(&api).await;
            if let Plan::Execute {
                command: ProtocolCommand::AcceptSecondaryRemovalCommit(c),
            } = plan
                && c.target.replica_id.value() == id
            {
                assert_eq!(
                    raw.set
                        .status
                        .unwrap()
                        .authority
                        .secondary_scale_down_cleanup,
                    Some(c.committed.clone())
                );
                break c.committed;
            }
            tick(&api).await;
        };
        let mut raw = api.observation().await;
        let key = ReplicaObservationKey::new(
            ReplicaId::new(id),
            ReplicaInstanceId::new(format!("pod-uid-{id}")),
        );
        let RawAgentObservation::Report(report) = raw.agents.get_mut(&key).unwrap() else {
            unreachable!()
        };
        report.process_session_id = format!("restarted-{id}");
        report.report_sequence = 1;
        let restarted = Arc::new(InMemoryClusterApi::new(raw));
        let (_, plan) = next_plan(&restarted).await;
        assert!(matches!(plan, Plan::Execute {
            command: ProtocolCommand::AcceptSecondaryRemovalCommit(c)
        } if c.target.replica_id.value() == id && c.committed == frozen));
        finish(&restarted).await;
        let raw = restarted.observation().await;
        let receipt = raw
            .set
            .status
            .unwrap()
            .authority
            .last_secondary_removal
            .unwrap();
        assert_eq!(receipt.committed(), frozen);
        let RawAgentObservation::Report(primary) = &raw.agents
            [&ReplicaObservationKey::new(ReplicaId::new(1), ReplicaInstanceId::new("pod-uid-1"))]
        else {
            unreachable!()
        };
        assert_eq!(primary.write_status, proto::AccessStatus::Granted as i32);
    }
}

fn replacement_at_commit() -> (RawObservation, ReplicaIdentity, ReplicaIdentity) {
    let mut raw = fixture(3, 3);
    let status = &mut raw.set.status.as_mut().unwrap().authority;
    let previous = status.topology.as_ref().unwrap().configuration.clone();
    let old = previous.members[1].identity.clone();
    let new = ReplicaIdentity {
        replica_id: old.replica_id,
        instance_id: ReplicaInstanceId::new("replacement-pod"),
        agent_generation: derive_agent_generation(&derive_initialization_id(
            &ResourceUid::new(UID),
            old.replica_id,
            &PodUid::new("replacement-pod"),
            &PvcUid::new("replacement-pvc"),
        )),
    };
    let mut members = previous.members.clone();
    members[1].identity = new.clone();
    let current = ConfigurationDescriptor::new(
        Epoch::new(0, 2),
        previous.primary_id,
        members,
        previous.write_quorum,
    );
    let transition_id = derive_transition_id(
        &ResourceUid::new(UID),
        TransitionKind::Replacement,
        &current.configuration_id,
    );
    status.transition = Some(TransitionIntent {
        transition_id: transition_id.clone(),
        kind: TransitionKind::Replacement,
        spec_generation: 2,
        effective_policy: status.effective_policy.clone().unwrap(),
        previous_configuration_id: Some(previous.configuration_id),
        current_configuration: current.clone(),
        election_lsn: None,
        build_id: Some(OperationId::new("first-replacement")),
        repair: None,
        switchover: None,
        secondary_scale_down: None,
        secondary_removal_evidence: None,
    });
    let mut pod = raw.pods[1].clone();
    pod.metadata.name = Some("db-replacement".into());
    pod.metadata.uid = Some(new.instance_id.to_string());
    pod.metadata
        .labels
        .as_mut()
        .unwrap()
        .insert(INSTANCE_LABEL.into(), new.instance_id.to_string());
    pod.spec.as_mut().unwrap().volumes.as_mut().unwrap()[0]
        .persistent_volume_claim
        .as_mut()
        .unwrap()
        .claim_name = "db-replacement-data".into();
    raw.pods.push(pod);
    let mut pvc = raw.pvcs[1].clone();
    pvc.metadata.name = Some("db-replacement-data".into());
    pvc.metadata.uid = Some("replacement-pvc".into());
    pvc.metadata
        .labels
        .as_mut()
        .unwrap()
        .insert(INSTANCE_LABEL.into(), new.instance_id.to_string());
    raw.pvcs.push(pvc);
    let mut endpoint = raw.services[1].clone();
    endpoint.metadata.name = Some(derive_replica_endpoint_name(&ResourceUid::new(UID), &new));
    endpoint.metadata.uid = Some("replacement-endpoint".into());
    endpoint.spec.as_mut().unwrap().selector = Some(BTreeMap::from([(
        INSTANCE_LABEL.into(),
        new.instance_id.to_string(),
    )]));
    raw.services.push(endpoint);
    raw.agents.remove(&ReplicaObservationKey::new(
        old.replica_id,
        old.instance_id.clone(),
    ));
    for member in &current.members {
        let mut report = initialized_report(member.identity.clone(), current.clone());
        report.replica_id = member.identity.replica_id.value();
        report.pod_uid = member.identity.instance_id.to_string();
        report.pvc_uid = if member.identity == new {
            "replacement-pvc".into()
        } else {
            format!("pvc-uid-{}", member.identity.replica_id)
        };
        report.process_session_id = format!("replacement-session-{}", member.identity.replica_id);
        report.role = if member.role == ReplicaRole::Primary {
            proto::ReplicaRole::Primary as i32
        } else {
            proto::ReplicaRole::ActiveSecondary as i32
        };
        report.write_status = if member.role == ReplicaRole::Primary {
            proto::AccessStatus::Granted as i32
        } else {
            proto::AccessStatus::NotPrimary as i32
        };
        report.retained_operation_id = format!(
            "{transition_id}:current-only:{}",
            member.identity.replica_id
        );
        raw.agents.insert(
            ReplicaObservationKey::new(
                member.identity.replica_id,
                member.identity.instance_id.clone(),
            ),
            RawAgentObservation::Report(Box::new(report)),
        );
    }
    (raw, old, new)
}

#[tokio::test]
async fn consecutive_replacement_failures_serialize_frozen_cleanup_across_restarts() {
    let (raw, old, new) = replacement_at_commit();
    let mut api = Arc::new(InMemoryClusterApi::new(raw));
    let receipt = loop {
        tick(&api).await;
        if let Some(receipt) = api
            .observation()
            .await
            .set
            .status
            .unwrap()
            .authority
            .last_replacement
        {
            assert_eq!(receipt.target, old);
            break receipt;
        }
    };
    let healthy = api.observation().await;
    let mut failed = healthy.clone();
    let RawAgentObservation::Report(report) = failed
        .agents
        .get_mut(&ReplicaObservationKey::new(
            new.replica_id,
            new.instance_id.clone(),
        ))
        .unwrap()
    else {
        unreachable!()
    };
    report.reported_fault = proto::FaultType::Permanent as i32;
    api.set_observation(failed).await;
    for (resource, identity) in [
        (ScaleDownResource::Endpoint, &receipt.resources.endpoint),
        (ScaleDownResource::Pod, &receipt.resources.pod),
        (ScaleDownResource::Pvc, &receipt.resources.pvc),
    ] {
        let mut raw = api.observation().await;
        metadata(&mut raw, resource, identity.name()).finalizers = Some(vec!["test/hold".into()]);
        api = Arc::new(InMemoryClusterApi::new(raw));
        for _ in 0..3 {
            let (_, effects) = tick(&api).await;
            assert!(
                matches!(effects.as_slice(), [EffectRecord::DeleteScaleDownResource { resource: r, .. }] if *r == resource)
            );
            let status = api.observation().await.set.status.unwrap().authority;
            assert_eq!(status.last_replacement, Some(receipt.clone()));
            assert!(status.provisioning.is_none() && status.transition.is_none());
        }
        let mut raw = api.observation().await;
        metadata(&mut raw, resource, identity.name()).finalizers = None;
        api.set_observation(raw).await;
        api.lose_next_delete_reply().await;
        assert_eq!(tick(&api).await.0, ReconcileKind::ObservationStale);
        api = Arc::new(InMemoryClusterApi::new(api.observation().await));
    }
    tick(&api).await;
    assert!(
        api.observation()
            .await
            .set
            .status
            .unwrap()
            .authority
            .last_replacement
            .is_none()
    );
    let raw = api.observation().await;
    assert!(
        raw.pods
            .iter()
            .any(|p| p.uid().as_deref() == Some(new.instance_id.as_str()))
    );
    assert!(
        raw.pvcs
            .iter()
            .any(|p| p.uid().as_deref() == Some("replacement-pvc"))
    );
    assert!(
        raw.services
            .iter()
            .any(|s| s.uid().as_deref() == Some("replacement-endpoint"))
    );
    assert!(
        !raw.pods
            .iter()
            .any(|p| p.uid().as_deref() == Some(old.instance_id.as_str()))
    );
    assert!(
        !raw.pvcs
            .iter()
            .any(|p| p.uid().as_deref() == Some("pvc-uid-2"))
    );
    assert!(
        !raw.services
            .iter()
            .any(|s| s.uid().as_deref() == Some("endpoint-2"))
    );
    let (_, plan) = next_plan(&api).await;
    assert!(
        matches!(plan, Plan::Apply { changes } if matches!(changes.as_slice(),
        [KubernetesChange::EnsureReplacementScaffolding { replacing, .. }] if replacing == &new))
    );
}

#[tokio::test]
async fn replacement_cleanup_does_not_adopt_churned_uids_or_list_absence() {
    for resource in [
        ScaleDownResource::Endpoint,
        ScaleDownResource::Pod,
        ScaleDownResource::Pvc,
    ] {
        let (raw, _, new) = replacement_at_commit();
        let api = Arc::new(InMemoryClusterApi::new(raw));
        let (observed, change) = at_delete(&api, resource).await;
        let KubernetesChange::DeleteScaleDownResource {
            name,
            uid,
            resource_version,
            ..
        } = change
        else {
            unreachable!()
        };
        let frozen = observed
            .set
            .status
            .as_ref()
            .unwrap()
            .authority
            .last_replacement
            .clone()
            .unwrap();
        let kind = match resource {
            ScaleDownResource::Endpoint => "Service",
            ScaleDownResource::Pod => "Pod",
            ScaleDownResource::Pvc => "PVC",
        };
        api.fail_exact_lookup(format!("{kind}/{name}"), Some("unavailable".into()))
            .await;
        assert_eq!(tick(&api).await.0, ReconcileKind::Waiting);
        assert_eq!(
            api.observation()
                .await
                .set
                .status
                .unwrap()
                .authority
                .last_replacement,
            Some(frozen.clone())
        );
        api.fail_exact_lookup(format!("{kind}/{name}"), None).await;
        let mut raw = api.observation().await;
        let meta = metadata(&mut raw, resource, &name);
        meta.uid = Some("unrelated-recreated-resource".into());
        meta.resource_version = Some("999".into());
        meta.labels = None;
        api.set_observation(raw).await;
        assert!(matches!(
            api.delete_scale_down_resource(&observed, resource, &name, &uid, &resource_version)
                .await,
            Err(ControllerError::ObservationStale)
        ));
        let restarted = Arc::new(InMemoryClusterApi::new(api.observation().await));
        finish(&restarted).await;
        let mut raw = restarted.observation().await;
        assert_eq!(
            metadata(&mut raw, resource, &name).uid.as_deref(),
            Some("unrelated-recreated-resource")
        );
        assert!(raw.set.status.unwrap().authority.last_replacement.is_none());
        assert!(
            raw.pods
                .iter()
                .any(|p| p.uid().as_deref() == Some(new.instance_id.as_str()))
        );
        assert!(
            raw.pvcs
                .iter()
                .any(|p| p.uid().as_deref() == Some("replacement-pvc"))
        );
        assert!(!restarted.effects().await.iter().any(|e| matches!(
            e,
            EffectRecord::DeleteScaffolding { .. } | EffectRecord::EnsureReplacement(_)
        )));
    }
}

#[tokio::test]
async fn same_name_replacements_and_rv_races_never_acquire_cleanup_authority() {
    for resource in [
        ScaleDownResource::Endpoint,
        ScaleDownResource::Pod,
        ScaleDownResource::Pvc,
    ] {
        for replace_before_observe in [true, false] {
            let api = Arc::new(InMemoryClusterApi::new(fixture(3, 2)));
            let (old, change) = at_delete(&api, resource).await;
            let KubernetesChange::DeleteScaleDownResource {
                name,
                uid,
                resource_version,
                ..
            } = change
            else {
                unreachable!()
            };
            let mut replaced = api.observation().await;
            let meta = metadata(&mut replaced, resource, &name);
            meta.uid = Some("replacement".into());
            meta.resource_version = Some("99".into());
            api.set_observation(replaced).await;
            assert_generic_cleanup_protected(&api, resource, &name).await;
            if !replace_before_observe {
                assert!(matches!(
                    api.delete_scale_down_resource(&old, resource, &name, &uid, &resource_version)
                        .await,
                    Err(ControllerError::ObservationStale)
                ));
            }
            finish(&api).await;
            for _ in 0..3 {
                tick(&api).await;
            }
            let mut result = api.observation().await;
            assert_eq!(
                metadata(&mut result, resource, &name).uid.as_deref(),
                Some("replacement")
            );
            assert_generic_cleanup_protected(&api, resource, &name).await;
            assert!(
                !api.effects()
                    .await
                    .iter()
                    .any(|e| matches!(e, EffectRecord::DeleteScaffolding { .. }))
            );
            assert!(matches!(
                api.delete_scale_down_resource(&old, resource, &name, &uid, &resource_version)
                    .await,
                Err(ControllerError::ObservationStale)
            ));
        }

        let api = Arc::new(InMemoryClusterApi::new(fixture(3, 2)));
        let (old, change) = at_delete(&api, resource).await;
        let KubernetesChange::DeleteScaleDownResource {
            name,
            uid,
            resource_version,
            ..
        } = change
        else {
            unreachable!()
        };
        let mut newer = api.observation().await;
        metadata(&mut newer, resource, &name).resource_version = Some("changed".into());
        api.set_observation(newer).await;
        assert!(matches!(
            api.delete_scale_down_resource(&old, resource, &name, &uid, &resource_version)
                .await,
            Err(ControllerError::ObservationStale)
        ));
        finish(&api).await;
    }
}

#[tokio::test]
async fn generic_cleanup_cannot_adopt_a_protected_replacement_endpoint() {
    let api = Arc::new(InMemoryClusterApi::new(fixture(3, 2)));
    finish(&api).await;
    let mut raw = api.observation().await;
    let receipt = raw
        .set
        .status
        .as_ref()
        .unwrap()
        .authority
        .last_secondary_removal
        .as_ref()
        .unwrap();
    let endpoint_name = receipt
        .evidence
        .preparation
        .intent
        .cleanup
        .endpoint
        .name()
        .to_string();
    raw.services.push(Service {
        metadata: kube::core::ObjectMeta {
            name: Some(endpoint_name.clone()),
            uid: Some("replacement-service-uid".into()),
            resource_version: Some("99".into()),
            labels: Some(BTreeMap::from([(SET_UID_LABEL.into(), UID.into())])),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([(
                INSTANCE_LABEL.into(),
                "replacement-pod-uid".into(),
            )])),
            ..Default::default()
        }),
        ..Default::default()
    });
    api.set_observation(raw).await;
    let observed = api.observation().await;

    assert!(matches!(
        api.delete_replica_scaffolding(
            &observed,
            Some("replacement-pod"),
            Some(&PodUid::new("replacement-pod-uid")),
            None,
            None,
        )
        .await,
        Err(ControllerError::ObservationStale)
    ));
    assert!(
        api.observation()
            .await
            .services
            .iter()
            .any(|service| service.name_any() == endpoint_name
                && service.uid().as_deref() == Some("replacement-service-uid"))
    );
    assert!(
        !api.effects()
            .await
            .iter()
            .any(|effect| matches!(effect, EffectRecord::DeleteScaffolding { .. }))
    );
}

async fn assert_generic_cleanup_protected(
    api: &InMemoryClusterApi,
    resource: ScaleDownResource,
    name: &str,
) {
    let raw = api.observation().await;
    let status = &raw.set.status.as_ref().unwrap().authority;
    let target = &status
        .secondary_scale_down_cleanup
        .as_ref()
        .map(|c| &c.evidence)
        .or_else(|| status.last_secondary_removal.as_ref().map(|r| &r.evidence))
        .unwrap()
        .preparation
        .intent
        .target;
    let result = match resource {
        ScaleDownResource::Pod => {
            assert!(matches!(
                api.delete_exact_pod(&raw, name, &PodUid::new("replacement"))
                    .await,
                Err(ControllerError::ObservationStale)
            ));
            api.delete_replica_scaffolding(
                &raw,
                Some(name),
                Some(&PodUid::new("replacement")),
                None,
                None,
            )
            .await
        }
        ScaleDownResource::Pvc => {
            api.delete_replica_scaffolding(
                &raw,
                None,
                None,
                Some(name),
                Some(&PvcUid::new("replacement")),
            )
            .await
        }
        ScaleDownResource::Endpoint => api.delete_replica_endpoint(&raw, target).await,
    };
    assert!(matches!(result, Err(ControllerError::ObservationStale)));
}

#[tokio::test]
async fn unavailable_target_label_loss_and_exact_lookup_failures_never_imply_absence() {
    let mut raw = fixture(2, 1);
    raw.agents.insert(
        ReplicaObservationKey::new(ReplicaId::new(2), ReplicaInstanceId::new("pod-uid-2")),
        RawAgentObservation::Unavailable {
            message: "partition".into(),
        },
    );
    let api = Arc::new(InMemoryClusterApi::new(raw));
    at_delete(&api, ScaleDownResource::Endpoint).await;
    let mut raw = api.observation().await;
    raw.pods[1].metadata.labels = None;
    api.set_observation(raw).await;
    let (raw, _) = next_plan(&api).await;
    let snapshot = normalize(raw, BTreeMap::new()).unwrap();
    assert!(matches!(
        snapshot.secondary_scale_down_resources[0].pod,
        ExactResourceObservation::FrozenUidPresent { .. }
    ));
    assert!(
        snapshot
            .observation_for_identity(&snapshot.secondary_scale_down_resources[0].target)
            .is_some()
    );
    api.fail_exact_lookup("Pod/db-2".into(), Some("403 forbidden".into()))
        .await;
    for _ in 0..2 {
        assert_eq!(tick(&api).await.0, ReconcileKind::Waiting);
        assert_eq!(api.observation().await.pods.len(), 2);
        assert_eq!(api.observation().await.pvcs.len(), 2);
    }
    let (raw, _) = next_plan(&api).await;
    let snapshot = normalize(raw, BTreeMap::new()).unwrap();
    assert!(matches!(
        snapshot.secondary_scale_down_resources[0].pod,
        ExactResourceObservation::LookupFailed { .. }
    ));
    assert!(
        snapshot
            .observation_failures
            .iter()
            .any(|f| f.source == "exact-Pod/db-2")
    );
    api.fail_exact_lookup("Pod/db-2".into(), None).await;
    finish(&api).await;
    assert!(
        !api.effects()
            .await
            .iter()
            .any(|e| matches!(e, EffectRecord::Execute(ProtocolCommand::RetireReplica(_))))
    );
    assert!(api.effects().await.iter().any(|e| matches!(e, EffectRecord::DeleteScaleDownResource { resource: ScaleDownResource::Pod, uid, .. } if uid == "pod-uid-2")));
}

#[tokio::test]
async fn authoritative_not_found_requires_no_target_retirement() {
    let mut raw = fixture(2, 1);
    raw.pods.pop();
    let api = Arc::new(InMemoryClusterApi::new(raw));
    let (observed, _) = next_plan(&api).await;
    assert!(matches!(
        normalize(observed, BTreeMap::new())
            .unwrap()
            .secondary_scale_down_resources[0]
            .pod,
        ExactResourceObservation::NotFound
    ));
    finish(&api).await;
    assert!(!api.effects().await.iter().any(|e| matches!(
        e,
        EffectRecord::Execute(ProtocolCommand::RetireReplica(_))
            | EffectRecord::DeleteScaleDownResource {
                resource: ScaleDownResource::Pod,
                ..
            }
    )));
}

#[tokio::test]
async fn ambiguous_commands_reobserve_exact_sessions_before_replay() {
    let api = Arc::new(InMemoryClusterApi::new(fixture(2, 1)));
    let mut seen = 0;
    for _ in 0..120 {
        let (raw, plan) = next_plan(&api).await;
        if let Plan::Execute { command } = plan {
            let before = api.effects().await.len();
            api.unavailable_next_execute().await;
            let action = Reconciler::new(api.clone(), enabled())
                .reconcile("tests", "db")
                .await
                .unwrap();
            assert_eq!(action.kind, ReconcileKind::Waiting);
            assert_eq!(api.effects().await.len(), before + 1);
            assert_eq!(
                next_plan(&api).await.1,
                Plan::Execute {
                    command: command.clone()
                }
            );
            let mut restarted = api.observation().await;
            for report in restarted.agents.values_mut() {
                if let RawAgentObservation::Report(report) = report {
                    report.process_session_id.push_str("-restart");
                }
            }
            api.set_observation(restarted).await;
            assert!(matches!(
                api.execute_command(&raw, &command).await,
                Err(ControllerError::ObservationStale)
            ));
            // The command can have applied despite reply loss; completion is only
            // supplied by the next independently observed process report.
            apply_command(&api, &command).await;
            seen += 1;
        } else if tick(&api).await.0 == ReconcileKind::Stable {
            assert!(seen >= 5);
            return;
        }
    }
    panic!("ambiguous command convergence failed")
}

#[tokio::test]
async fn completed_receipt_corrects_late_retained_member_without_deletion_authority() {
    let original = fixture(5, 4);
    let key = ReplicaObservationKey::new(ReplicaId::new(3), ReplicaInstanceId::new("pod-uid-3"));
    let late = original.agents[&key].clone();
    let api = Arc::new(InMemoryClusterApi::new(original));
    for _ in 0..50 {
        tick(&api).await;
        let mut raw = api.observation().await;
        if raw
            .set
            .status
            .as_ref()
            .unwrap()
            .authority
            .transition
            .as_ref()
            .is_some_and(|t| t.secondary_removal_evidence.is_some())
        {
            raw.agents.insert(
                key.clone(),
                RawAgentObservation::Unavailable {
                    message: "retained member partition".into(),
                },
            );
            api.set_observation(raw).await;
            break;
        }
    }
    finish(&api).await;
    let completed = api.observation().await;
    let receipt = completed
        .set
        .status
        .as_ref()
        .unwrap()
        .authority
        .last_secondary_removal
        .clone()
        .unwrap();
    assert!(
        !receipt
            .current_only_write_quorum
            .iter()
            .any(|w| w.identity.replica_id == ReplicaId::new(3))
    );
    let mut returning = completed.clone();
    returning.agents.insert(key.clone(), late);
    let restarted = Arc::new(InMemoryClusterApi::new(returning));
    finish(&restarted).await;
    let final_raw = restarted.observation().await;
    assert_eq!(final_raw.pods, completed.pods);
    assert_eq!(final_raw.pvcs, completed.pvcs);
    assert_eq!(final_raw.services, completed.services);
    assert_eq!(
        final_raw
            .set
            .status
            .unwrap()
            .authority
            .last_secondary_removal,
        Some(receipt)
    );
    let commands = restarted.effects().await;
    assert!(commands.iter().any(|e| matches!(e, EffectRecord::Execute(ProtocolCommand::EnsureConfiguration(c)) if c.local_replica_id == ReplicaId::new(3) && !c.current_only)));
    assert!(commands.iter().any(|e| matches!(e, EffectRecord::Execute(ProtocolCommand::AcceptSecondaryRemovalCommit(c)) if c.target.replica_id == ReplicaId::new(3))));
    assert!(!commands.iter().any(|e| matches!(
        e,
        EffectRecord::DeleteScaleDownResource { .. } | EffectRecord::DeleteScaffolding { .. }
    )));
}

#[tokio::test]
async fn exact_get_and_list_failures_block_each_cleanup_kind_until_observed() {
    for (resource, kind) in [
        (ScaleDownResource::Pod, "Pod"),
        (ScaleDownResource::Pvc, "PVC"),
        (ScaleDownResource::Endpoint, "Service"),
    ] {
        let baseline = Arc::new(InMemoryClusterApi::new(fixture(3, 2)));
        let (_, change) = at_delete(&baseline, resource).await;
        let KubernetesChange::DeleteScaleDownResource { name, .. } = change else {
            unreachable!()
        };
        let api = Arc::new(InMemoryClusterApi::new(baseline.observation().await));
        api.fail_exact_lookup(format!("{kind}/{name}"), Some("lookup timeout".into()))
            .await;
        for _ in 0..2 {
            assert_eq!(tick(&api).await.0, ReconcileKind::Waiting);
        }
        assert!(
            !api.effects()
                .await
                .iter()
                .any(|e| matches!(e, EffectRecord::DeleteScaleDownResource { .. }))
        );
        api.fail_exact_lookup(format!("{kind}/{name}"), None).await;
        let mut raw = api.observation().await;
        raw.failures.push(RawObservationFailure {
            source: "pods".into(),
            message: "list 403".into(),
        });
        api.set_observation(raw).await;
        assert_eq!(tick(&api).await.0, ReconcileKind::Waiting);
        let mut raw = api.observation().await;
        raw.failures.clear();
        api.set_observation(raw).await;
        finish(&api).await;
    }
}

#[tokio::test]
async fn exact_observation_preserves_unlabelled_target_and_simultaneous_incarnations() {
    let api = Arc::new(InMemoryClusterApi::new(fixture(3, 2)));
    at_delete(&api, ScaleDownResource::Endpoint).await;
    let mut raw = api.observation().await;
    let mut extra = raw.pods[2].clone();
    extra.metadata.name = Some("extra-target".into());
    extra.metadata.uid = Some("extra-uid".into());
    raw.pods[2].metadata.labels = None;
    raw.pvcs[2].metadata.labels = None;
    raw.pods.push(extra);
    api.set_observation(raw).await;
    let snapshot = normalize(api.observe("tests", "db").await.unwrap(), BTreeMap::new()).unwrap();
    assert!(matches!(
        &snapshot
            .observation_for_identity(&snapshot.secondary_scale_down_resources[0].target)
            .unwrap()
            .agent,
        AgentObservation::Report(_)
    ));
    assert_eq!(
        snapshot
            .replicas
            .keys()
            .filter(|k| k.replica_id == ReplicaId::new(3))
            .count(),
        2
    );
    finish(&api).await;
    assert!(
        api.observation()
            .await
            .pods
            .iter()
            .any(|p| p.uid().as_deref() == Some("extra-uid"))
    );
}

#[tokio::test]
async fn missing_storage_mapping_or_exact_metadata_never_freezes_cleanup() {
    for mutation in 0..3 {
        let mut raw = fixture(3, 2);
        match mutation {
            0 => raw.pods[2].spec.as_mut().unwrap().volumes = None,
            1 => raw.pvcs[2].metadata.uid = Some("unproven-storage".into()),
            _ => raw.pods[2].metadata.resource_version = None,
        }
        let api = Arc::new(InMemoryClusterApi::new(raw));
        for _ in 0..3 {
            tick(&api).await;
        }
        assert!(
            api.observation()
                .await
                .set
                .status
                .unwrap()
                .authority
                .transition
                .is_none()
        );
        assert!(!api.effects().await.iter().any(|e| matches!(
            e,
            EffectRecord::Execute(_) | EffectRecord::DeleteScaleDownResource { .. }
        )));
    }
}
#[tokio::test]
async fn every_status_and_command_boundary_survives_conflict_loss_and_controller_restart() {
    for (size, desired) in [(2, 1), (3, 2), (5, 2)] {
        let baseline = Arc::new(InMemoryClusterApi::new(fixture(size, desired)));
        let mut statuses = 0;
        let mut commands = 0;
        for boundary in 0..250 {
            let (raw, plan) = next_plan(&baseline).await;
            if matches!(plan, Plan::Stable { .. }) {
                break;
            }
            let mut persisted = raw.clone();
            persisted.set =
                serde_json::from_slice(&serde_json::to_vec(&persisted.set).unwrap()).unwrap();
            let fork = Arc::new(InMemoryClusterApi::new(persisted));
            match &plan {
                Plan::Apply { changes }
                    if matches!(
                        changes.first(),
                        Some(KubernetesChange::PersistStatus { .. })
                    ) =>
                {
                    fork.conflict_next_status().await;
                    let (kind, effects) = tick(&fork).await;
                    assert_eq!(
                        kind,
                        ReconcileKind::ObservationStale,
                        "size={size} boundary={boundary}"
                    );
                    assert!(effects.is_empty());
                    assert_eq!(fork.observation().await.set.status, raw.set.status);
                    assert_eq!(fork.observation().await.pods, raw.pods);
                    assert_eq!(fork.observation().await.pvcs, raw.pvcs);
                    statuses += 1;
                }
                Plan::Execute { command } => {
                    fork.unavailable_next_execute().await;
                    let result = Reconciler::new(fork.clone(), enabled())
                        .reconcile("tests", "db")
                        .await
                        .unwrap();
                    assert_eq!(result.kind, ReconcileKind::Waiting);
                    assert_eq!(
                        next_plan(&fork).await.1,
                        plan,
                        "undelivered request retains exact command"
                    );
                    // The same ambiguity also permits a completed effect with no reply.
                    apply_command(&fork, command).await;
                    assert_eq!(fork.observation().await.set.status, raw.set.status);
                    commands += 1;
                }
                _ => {}
            }
            let restarted = Arc::new(InMemoryClusterApi::new(fork.observation().await));
            finish(&restarted).await;
            let final_raw = restarted.observation().await;
            let status = final_raw.set.status.unwrap().authority;
            assert_eq!(
                status.topology.unwrap().configuration.members.len(),
                desired as usize
            );
            assert!(status.transition.is_none() && status.secondary_scale_down_cleanup.is_none());
            assert_eq!(final_raw.pods.len(), desired as usize);
            assert_eq!(final_raw.pvcs.len(), desired as usize);
            tick(&baseline).await;
        }
        assert!(
            statuses >= 4,
            "intent/evidence/commit/cleanup status boundaries"
        );
        assert!(
            commands >= 5,
            "prepare/member/current-only/accept/retire boundaries"
        );
    }
}

#[tokio::test]
async fn primary_or_retained_loss_at_every_precommit_boundary_preserves_authority_then_heals() {
    let baseline = Arc::new(InMemoryClusterApi::new(fixture(3, 2)));
    tick(&baseline).await;
    let mut boundaries = 0;
    for _ in 0..80 {
        let (raw, _) = next_plan(&baseline).await;
        let before = raw.set.status.as_ref().unwrap().authority.clone();
        if before.secondary_scale_down_cleanup.is_some() {
            break;
        }
        for id in [1, 2] {
            let mut lost = raw.clone();
            let key = ReplicaObservationKey::new(
                ReplicaId::new(id),
                ReplicaInstanceId::new(format!("pod-uid-{id}")),
            );
            let original = lost
                .agents
                .insert(
                    key.clone(),
                    RawAgentObservation::Unavailable {
                        message: "exact retained process lost".into(),
                    },
                )
                .unwrap();
            let api = Arc::new(InMemoryClusterApi::new(lost));
            for _ in 0..8 {
                tick(&api).await;
                let after = api.observation().await;
                let status = after.set.status.unwrap().authority;
                assert_eq!(status.topology, before.topology);
                assert_eq!(status.effective_policy, before.effective_policy);
                assert_eq!(
                    status.transition.as_ref().unwrap().secondary_scale_down,
                    before.transition.as_ref().unwrap().secondary_scale_down
                );
                assert!(status.secondary_scale_down_cleanup.is_none());
                assert_eq!(after.pods, raw.pods);
                assert_eq!(after.pvcs, raw.pvcs);
                assert!(!api.effects().await.iter().any(|e| matches!(
                    e,
                    EffectRecord::DeleteScaleDownResource { .. }
                        | EffectRecord::DeleteScaffolding { .. }
                )));
            }
            let mut healed = api.observation().await;
            healed.agents.insert(key, original);
            let restarted = Arc::new(InMemoryClusterApi::new(healed));
            finish(&restarted).await;
            assert_eq!(restarted.observation().await.pods.len(), 2);
        }
        boundaries += 1;
        tick(&baseline).await;
    }
    assert!(boundaries >= 8);
}

#[tokio::test]
async fn target_return_after_commit_is_retired_before_sequential_next_removal() {
    let mut original = fixture(3, 1);
    let key = ReplicaObservationKey::new(ReplicaId::new(3), ReplicaInstanceId::new("pod-uid-3"));
    let target = original
        .agents
        .insert(
            key.clone(),
            RawAgentObservation::Unavailable {
                message: "partitioned target".into(),
            },
        )
        .unwrap();
    let api = Arc::new(InMemoryClusterApi::new(original));
    at_delete(&api, ScaleDownResource::Endpoint).await;
    let mut returned = api.observation().await;
    let accepted = returned
        .set
        .status
        .as_ref()
        .unwrap()
        .authority
        .topology
        .clone();
    assert_eq!(accepted.as_ref().unwrap().configuration.members.len(), 2);
    let RawAgentObservation::Report(mut target) = target else {
        unreachable!()
    };
    target.process_session_id = "returned-new-session".into();
    target.report_sequence = 1;
    returned
        .agents
        .insert(key, RawAgentObservation::Report(target));
    let restarted = Arc::new(InMemoryClusterApi::new(returned));
    finish(&restarted).await;
    let effects = restarted.effects().await;
    let retired = effects.iter().position(|e| matches!(e,
                EffectRecord::Execute(ProtocolCommand::RetireReplica(c)) if c.local_replica_id == ReplicaId::new(3))).unwrap();
    let pvc = effects.iter().position(|e| matches!(e,
                EffectRecord::DeleteScaleDownResource { resource: ScaleDownResource::Pvc, uid, .. } if uid == "pvc-uid-3")).unwrap();
    let next = effects.iter().position(|e| matches!(e,
                EffectRecord::Execute(ProtocolCommand::PrepareSecondaryRemoval(c)) if c.intent.target.replica_id == ReplicaId::new(2))).unwrap();
    assert!(retired < pvc && pvc < next);
    assert_eq!(restarted.observation().await.pods.len(), 1);
    assert!(!effects.iter().any(|e| matches!(e,
                EffectRecord::Execute(ProtocolCommand::EnsureConfiguration(c)) if c.local_replica_id == ReplicaId::new(3))));
}

#[tokio::test]
async fn contradictory_current_only_reports_cannot_publish_reduced_status() {
    let baseline = Arc::new(InMemoryClusterApi::new(fixture(3, 2)));
    for _ in 0..80 {
        let (_, plan) = next_plan(&baseline).await;
        if matches!(plan, Plan::Apply { changes }
                    if matches!(changes.first(), Some(KubernetesChange::PersistStatus { status })
                        if status.secondary_scale_down_cleanup.is_some()))
        {
            break;
        }
        tick(&baseline).await;
    }
    let original = baseline.observation().await;
    let before = original.set.status.as_ref().unwrap().authority.clone();
    let key = ReplicaObservationKey::new(ReplicaId::new(1), ReplicaInstanceId::new("pod-uid-1"));
    for mutation in 0..4 {
        let mut broken = original.clone();
        let RawAgentObservation::Report(report) = broken.agents.get_mut(&key).unwrap() else {
            unreachable!()
        };
        match mutation {
            0 => report.verified_replication_lsn = None,
            1 => report.pending_operation_id = "unrelated-command".into(),
            2 => report.epoch.as_mut().unwrap().configuration_number += 1,
            _ => report.identity.as_mut().unwrap().agent_generation = "different-generation".into(),
        }
        let api = Arc::new(InMemoryClusterApi::new(broken));
        for _ in 0..3 {
            let count = api.observation_count().await;
            Reconciler::new(api.clone(), enabled())
                .reconcile("tests", "db")
                .await
                .unwrap();
            assert_eq!(api.observation_count().await, count + 1);
            assert!(api.effects().await.iter().all(|e| matches!(
                e,
                EffectRecord::RemoveWriteRouting | EffectRecord::ReplaceStatus
            )));
            let observed = api.observation().await;
            let status = observed.set.status.unwrap().authority;
            assert_eq!(status.topology, before.topology, "mutation={mutation}");
            assert_eq!(status.transition, before.transition);
            assert!(status.secondary_scale_down_cleanup.is_none());
            assert_eq!(observed.pods, original.pods);
            assert_eq!(observed.pvcs, original.pvcs);
            assert!(!api.effects().await.iter().any(|e| matches!(
                e,
                EffectRecord::DeleteScaleDownResource { .. }
                    | EffectRecord::DeleteScaffolding { .. }
            )));
        }
        let mut healed = api.observation().await;
        healed
            .agents
            .insert(key.clone(), original.agents[&key].clone());
        let restarted = Arc::new(InMemoryClusterApi::new(healed));
        finish(&restarted).await;
        assert_eq!(restarted.observation().await.pods.len(), 2);
    }
}
