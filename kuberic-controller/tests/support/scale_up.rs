use super::*;
use kube::Resource;
use kuberic_protocol::command::{KubernetesChange, ScaleDownResource};
use kuberic_protocol::types::{AccessStatus, Epoch, TransitionKind};

fn enabled() -> EvaluationConfig {
    EvaluationConfig {
        enable_secondary_scale_down: true,
        allow_scale_up: true,
        ..config()
    }
}

fn fixture(count: u32, desired: u32) -> RawObservation {
    secondary_scale_down::fixture(count, desired)
}

fn accepted_count(raw: &RawObservation) -> u32 {
    raw.set
        .status
        .as_ref()
        .and_then(|status| status.authority.effective_policy.as_ref())
        .map_or(0, |policy| policy.replica_set_size)
}

fn candidate_key(raw: &RawObservation) -> Option<(ReplicaObservationKey, String)> {
    let accepted = accepted_count(raw);
    raw.pods.iter().find_map(|pod| {
        let replica_id = pod
            .labels()
            .get(REPLICA_ID_LABEL)?
            .parse::<i64>()
            .ok()
            .map(ReplicaId::new)?;
        if replica_id.value() <= i64::from(accepted) {
            return None;
        }
        let pod_uid = pod.uid()?;
        let pvc_name = pod
            .spec
            .as_ref()?
            .volumes
            .as_ref()?
            .iter()
            .find_map(|volume| {
                volume
                    .persistent_volume_claim
                    .as_ref()
                    .map(|claim| claim.claim_name.clone())
            })?;
        let pvc_uid = raw
            .pvcs
            .iter()
            .find(|pvc| pvc.name_any() == pvc_name)?
            .uid()?;
        Some((
            ReplicaObservationKey::new(replica_id, ReplicaInstanceId::new(&pod_uid)),
            pvc_uid,
        ))
    })
}

async fn observe_fresh_candidate(api: &InMemoryClusterApi) {
    let mut raw = api.observation().await;
    let Some((key, pvc_uid)) = candidate_key(&raw) else {
        return;
    };
    if raw.agents.contains_key(&key) {
        return;
    }
    raw.agents.insert(
        key.clone(),
        RawAgentObservation::Report(Box::new(proto::AgentStatusReport {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            resource_uid: UID.to_string(),
            process_session_id: format!("fresh-session-{}", key.replica_id),
            report_sequence: 1,
            storage_state: proto::AgentStorageState::Uninitialized as i32,
            pod_uid: key.instance_id.to_string(),
            pvc_uid,
            replica_id: key.replica_id.value(),
            ..Default::default()
        })),
    );
    api.set_observation(raw).await;
}

async fn apply_command(api: &InMemoryClusterApi, command: &ProtocolCommand) {
    let mut raw = api.observation().await;
    match command {
        ProtocolCommand::InitializeAgentStore(command) => {
            let target = ReplicaIdentity {
                replica_id: command.local_replica_id,
                instance_id: command.expected_instance_id.clone(),
                agent_generation: command.assigned_agent_generation.clone(),
            };
            let key = ReplicaObservationKey::new(target.replica_id, target.instance_id.clone());
            let previous = match raw.agents.get(&key) {
                Some(RawAgentObservation::Report(report)) => report.report_sequence,
                _ => 0,
            };
            raw.agents.insert(
                key,
                RawAgentObservation::Report(Box::new(proto::AgentStatusReport {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    resource_uid: UID.to_string(),
                    identity: Some(target.into()),
                    process_session_id: format!("initialized-session-{}", command.local_replica_id),
                    report_sequence: previous + 1,
                    role: proto::ReplicaRole::None as i32,
                    read_status: proto::AccessStatus::NotPrimary as i32,
                    write_status: proto::AccessStatus::NotPrimary as i32,
                    epoch: Some(Epoch::default().into()),
                    storage_state: proto::AgentStorageState::Initialized as i32,
                    healthy: true,
                    replica_id: command.local_replica_id.value(),
                    ..Default::default()
                })),
            );
        }
        ProtocolCommand::EnsureReplicaBuild(command) => {
            let source_key = ReplicaObservationKey::new(
                command.local_replica_id,
                command.expected_instance_id.clone(),
            );
            let target_key = ReplicaObservationKey::new(
                command.target.replica_id,
                command.target.instance_id.clone(),
            );
            let boundary = match raw.agents.get(&source_key) {
                Some(RawAgentObservation::Report(report)) => report.current_progress,
                _ => panic!("scale-up source report"),
            };
            let catch_up_boundary = boundary + 2;
            let build = proto::BuildStatus {
                build_id: command.operation_id.to_string(),
                target: Some(command.target.clone().into()),
                last_sequence: 2,
                durable_lsn: catch_up_boundary,
                completed: true,
                catch_up_boundary_lsn: Some(catch_up_boundary),
                replication_boundary_lsn: boundary,
            };
            for key in [&source_key, &target_key] {
                let RawAgentObservation::Report(report) =
                    raw.agents.get_mut(key).expect("exact build participant")
                else {
                    panic!("exact build participant report")
                };
                report.current_progress = catch_up_boundary;
                report.committed_lsn = catch_up_boundary;
                report.builds = vec![build.clone()];
                report.report_sequence += 1;
                if key == &target_key {
                    report.role = proto::ReplicaRole::IdleSecondary as i32;
                }
            }
        }
        ProtocolCommand::EnsureConfiguration(command) => {
            let target = ReplicaIdentity {
                replica_id: command.local_replica_id,
                instance_id: command.expected_instance_id.clone(),
                agent_generation: command.expected_agent_generation.clone(),
            };
            let key = ReplicaObservationKey::new(target.replica_id, target.instance_id.clone());
            let RawAgentObservation::Report(report) =
                raw.agents.get_mut(&key).expect("configuration participant")
            else {
                panic!("configuration participant report")
            };
            let member = command
                .current_configuration
                .members
                .iter()
                .find(|member| member.identity == target)
                .expect("configuration contains local target");
            let boundary = command
                .scale_up_evidence
                .as_deref()
                .map_or(report.current_progress, |evidence| {
                    evidence.intent().catch_up_boundary_lsn
                });
            report.role = match member.role {
                ReplicaRole::Primary => proto::ReplicaRole::Primary as i32,
                ReplicaRole::ActiveSecondary => proto::ReplicaRole::ActiveSecondary as i32,
                ReplicaRole::IdleSecondary => proto::ReplicaRole::IdleSecondary as i32,
                ReplicaRole::None => proto::ReplicaRole::None as i32,
            };
            report.read_status = proto::AccessStatus::Granted as i32;
            report.write_status = if member.role == ReplicaRole::Primary {
                match command.primary_write_status {
                    AccessStatus::Granted => proto::AccessStatus::Granted as i32,
                    AccessStatus::ReconfigurationPending => {
                        proto::AccessStatus::ReconfigurationPending as i32
                    }
                    AccessStatus::NoWriteQuorum => proto::AccessStatus::NoWriteQuorum as i32,
                    AccessStatus::NotPrimary => proto::AccessStatus::NotPrimary as i32,
                }
            } else {
                proto::AccessStatus::NotPrimary as i32
            };
            report.epoch = Some(command.current_epoch.into());
            report.previous_configuration = command.previous_configuration.clone().map(Into::into);
            report.current_configuration = Some(command.current_configuration.clone().into());
            report.current_progress = report.current_progress.max(boundary);
            report.committed_lsn = report.committed_lsn.max(boundary);
            report.verified_replication_lsn = Some(boundary);
            report.current_configuration_quorum_progress = boundary;
            report.catch_up_boundary = Some(boundary);
            report.catch_up_complete = true;
            report.pending_operation_id.clear();
            report.retained_operation_id = command.operation_id.to_string();
            report.scale_up_intent = command
                .scale_up_evidence
                .as_deref()
                .map(|evidence| evidence.intent().clone().into());
            if command.transition_kind == TransitionKind::Failover {
                report.deactivation_epoch = Some(command.current_epoch.into());
                report.deactivated_lsn = Some(boundary);
            }
            report.report_sequence += 1;
        }
        ProtocolCommand::PrepareSwitchover(_) => {}
        other => panic!("unexpected scale-up command {other:?}"),
    }
    api.set_observation(raw).await;
}

async fn tick(api: &Arc<InMemoryClusterApi>) -> (ReconcileKind, Vec<EffectRecord>) {
    observe_fresh_candidate(api).await;
    let mut restored = api.observation().await;
    restored.set = serde_json::from_value(serde_json::to_value(&restored.set).unwrap()).unwrap();
    api.set_observation(restored).await;
    let before = api.effects().await.len();
    let observed = api.observation_count().await;
    let action = Reconciler::new(api.clone(), enabled())
        .reconcile("tests", "db")
        .await
        .unwrap();
    assert_eq!(api.observation_count().await, observed + 1);
    let effects = api.effects().await[before..].to_vec();
    assert!(
        effects.len() <= 1
            || matches!(
                effects.as_slice(),
                [
                    EffectRecord::RemoveWriteRouting,
                    EffectRecord::ReplaceStatus
                ]
            ),
        "at most one authority command per fresh observation: {effects:?}"
    );
    for effect in &effects {
        if let EffectRecord::Execute(command) = effect {
            apply_command(api, command).await;
        }
    }
    (action.kind, effects)
}

async fn finish(api: &Arc<InMemoryClusterApi>, desired: u32) -> Vec<(String, String)> {
    let mut diagnostics = BTreeMap::new();
    for _ in 0..300 {
        let (kind, _) = tick(api).await;
        let raw = api.observation().await;
        if let Some(status) = &raw.set.status {
            for condition in &status.authority.conditions {
                if condition.reason.starts_with("ScaleUp") {
                    diagnostics.insert(condition.reason.clone(), condition.message.clone());
                }
            }
            if kind == ReconcileKind::Stable
                && accepted_count(&raw) == desired
                && status.authority.provisioning.is_none()
                && status.authority.transition.is_none()
            {
                return diagnostics.into_iter().collect();
            }
        }
    }
    panic!(
        "scale-up did not converge: {:?}",
        api.observation().await.set.status
    );
}

async fn drive_until_cleanup(api: &Arc<InMemoryClusterApi>) {
    for _ in 0..80 {
        tick(api).await;
        if api
            .observation()
            .await
            .set
            .status
            .as_ref()
            .is_some_and(|status| status.authority.scale_up_cleanup.is_some())
        {
            return;
        }
        let mut raw = api.observation().await;
        if raw
            .set
            .status
            .as_ref()
            .is_some_and(|status| status.authority.provisioning.is_some())
            && candidate_key(&raw).is_some_and(|(key, _)| {
                raw.services.iter().any(|service| {
                    service
                        .spec
                        .as_ref()
                        .and_then(|spec| spec.selector.as_ref())
                        .and_then(|selector| selector.get(INSTANCE_LABEL))
                        .map(String::as_str)
                        == Some(key.instance_id.as_str())
                })
            })
        {
            raw.set.spec.replicas = accepted_count(&raw);
            raw.set.metadata.generation = Some(raw.set.metadata.generation.unwrap_or_default() + 1);
            api.set_observation(raw).await;
        }
    }
    panic!("scale-up cleanup was not frozen");
}

#[tokio::test]
async fn canonical_candidate_is_created_pvc_before_pod_and_lost_create_replays() {
    let api = Arc::new(InMemoryClusterApi::new(fixture(1, 2)));
    api.lose_next_create_reply().await;
    assert_eq!(tick(&api).await.0, ReconcileKind::ObservationStale);
    let after_lost_reply = api.observation().await;
    assert!(
        after_lost_reply
            .pvcs
            .iter()
            .any(|pvc| pvc.name_any() == "db-2-data")
    );
    assert!(
        after_lost_reply
            .pods
            .iter()
            .all(|pod| pod.name_any() != "db-2")
    );

    tick(&api).await;
    let after_pod = api.observation().await;
    let pod = after_pod
        .pods
        .iter()
        .find(|pod| pod.name_any() == "db-2")
        .expect("canonical candidate Pod");
    assert_eq!(
        pod.spec.as_ref().unwrap().volumes.as_ref().unwrap()[0]
            .persistent_volume_claim
            .as_ref()
            .unwrap()
            .claim_name,
        "db-2-data"
    );
    assert_eq!(
        pod.spec.as_ref().unwrap().containers[0].image.as_deref(),
        Some("example/db:latest")
    );
}

#[tokio::test]
async fn controller_converges_sequential_scale_up_through_exact_agent_commands() {
    let api = Arc::new(InMemoryClusterApi::new(fixture(1, 3)));
    let diagnostics = finish(&api, 3).await;
    let raw = api.observation().await;
    let status = &raw.set.status.as_ref().unwrap().authority;
    assert_eq!(
        status
            .topology
            .as_ref()
            .unwrap()
            .configuration
            .members
            .iter()
            .map(|member| member.identity.replica_id.value())
            .collect::<Vec<_>>(),
        vec![1, 2, 3]
    );
    assert!(status.last_scale_up.is_some());
    assert!(status.scale_up_cleanup.is_none());
    for id in 2..=3 {
        assert!(
            raw.pvcs
                .iter()
                .any(|pvc| pvc.name_any() == format!("db-{id}-data"))
        );
        assert!(
            raw.pods
                .iter()
                .any(|pod| pod.name_any() == format!("db-{id}"))
        );
    }
    let effects = api.effects().await;
    let initialized = effects
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                EffectRecord::Execute(ProtocolCommand::InitializeAgentStore(_))
            )
        })
        .count();
    let builds = effects
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                EffectRecord::Execute(ProtocolCommand::EnsureReplicaBuild(_))
            )
        })
        .count();
    let configurations = effects
        .iter()
        .filter(|effect| {
            matches!(
                effect,
                EffectRecord::Execute(ProtocolCommand::EnsureConfiguration(command))
                    if command.scale_up_evidence.is_some()
            )
        })
        .count();
    assert_eq!(initialized, 2);
    assert_eq!(builds, 2);
    assert!(configurations >= 6);
    for (reason, message) in &diagnostics {
        for field in [
            "accepted=",
            "desired=",
            "target=",
            "attempt=",
            "phase=",
            "blocking=",
        ] {
            assert!(
                message.contains(field),
                "{reason} omitted {field}: {message}"
            );
        }
    }
    for reason in [
        "ScaleUpProvisioningAccepted",
        "ScaleUpCopying",
        "ScaleUpPreviousCurrentPersisted",
        "ScaleUpStable",
    ] {
        assert!(
            diagnostics.iter().any(|(candidate, _)| candidate == reason),
            "missing controller-visible phase {reason}: {diagnostics:?}"
        );
    }
}

#[tokio::test]
async fn scale_up_reobserves_status_conflict_and_lost_agent_replies() {
    let api = Arc::new(InMemoryClusterApi::new(fixture(1, 2)));
    let mut conflicted = false;
    for _ in 0..20 {
        observe_fresh_candidate(&api).await;
        let raw = api.observe("tests", "db").await.unwrap();
        let plan = evaluate(&normalize(raw, BTreeMap::new()).unwrap(), &enabled());
        if matches!(
            plan,
            Plan::Apply { ref changes }
                if changes.iter().any(|change| {
                    matches!(
                        change,
                        KubernetesChange::PersistStatus { status }
                            if status.provisioning.is_some()
                    )
                })
        ) {
            api.conflict_next_status().await;
            let (kind, effects) = tick(&api).await;
            assert_eq!(kind, ReconcileKind::ObservationStale);
            assert!(
                effects
                    .iter()
                    .all(|effect| !matches!(effect, EffectRecord::Execute(_)))
            );
            conflicted = true;
            break;
        }
        tick(&api).await;
    }
    assert!(conflicted, "scale-up provisioning status was not reached");

    let mut lost_initialize = false;
    let mut lost_build = false;
    let mut lost_configuration = false;
    for _ in 0..200 {
        observe_fresh_candidate(&api).await;
        let raw = api.observe("tests", "db").await.unwrap();
        let plan = evaluate(&normalize(raw, BTreeMap::new()).unwrap(), &enabled());
        let inject = match &plan {
            Plan::Execute {
                command: ProtocolCommand::InitializeAgentStore(_),
            } if !lost_initialize => {
                lost_initialize = true;
                true
            }
            Plan::Execute {
                command: ProtocolCommand::EnsureReplicaBuild(_),
            } if !lost_build => {
                lost_build = true;
                true
            }
            Plan::Execute {
                command: ProtocolCommand::EnsureConfiguration(command),
            } if command.scale_up_evidence.is_some() && !lost_configuration => {
                lost_configuration = true;
                true
            }
            _ => false,
        };
        if inject {
            api.unavailable_next_execute().await;
        }
        let (kind, effects) = tick(&api).await;
        if inject {
            assert_eq!(kind, ReconcileKind::Waiting);
            assert!(
                effects
                    .iter()
                    .any(|effect| matches!(effect, EffectRecord::Execute(_)))
            );
        }
        if kind == ReconcileKind::Stable && accepted_count(&api.observation().await) == 2 {
            break;
        }
    }
    assert!(lost_initialize && lost_build && lost_configuration);
    finish(&api, 2).await;
}

#[tokio::test]
async fn cancelled_candidate_cleanup_is_exact_ordered_and_restart_safe() {
    let api = Arc::new(InMemoryClusterApi::new(fixture(1, 2)));
    drive_until_cleanup(&api).await;
    let before_cleanup = api.observation().await;
    let old_pod_uid = before_cleanup
        .pods
        .iter()
        .find(|pod| pod.name_any() == "db-2")
        .and_then(ResourceExt::uid)
        .unwrap();
    let old_pvc_uid = before_cleanup
        .pvcs
        .iter()
        .find(|pvc| pvc.name_any() == "db-2-data")
        .and_then(ResourceExt::uid)
        .unwrap();
    api.lose_next_delete_reply().await;
    let diagnostics = finish(&api, 1).await;
    let raw = api.observation().await;
    assert!(raw.pods.iter().all(|pod| pod.name_any() != "db-2"));
    assert!(raw.pvcs.iter().all(|pvc| pvc.name_any() != "db-2-data"));
    let deleted = api
        .effects()
        .await
        .into_iter()
        .filter_map(|effect| match effect {
            EffectRecord::DeleteScaleDownResource { resource, .. } => Some(resource),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        deleted,
        vec![
            ScaleDownResource::Endpoint,
            ScaleDownResource::Pod,
            ScaleDownResource::Pvc
        ]
    );
    assert!(
        diagnostics
            .iter()
            .any(|(reason, _)| reason == "ScaleUpCleanupComplete")
    );

    let mut retry = api.observation().await;
    retry.set.spec.replicas = 2;
    retry.set.metadata.generation = Some(retry.set.metadata.generation.unwrap_or_default() + 1);
    api.set_observation(retry).await;
    finish(&api, 2).await;
    let retried = api.observation().await;
    assert_ne!(
        retried
            .pods
            .iter()
            .find(|pod| pod.name_any() == "db-2")
            .and_then(ResourceExt::uid)
            .unwrap(),
        old_pod_uid
    );
    assert_ne!(
        retried
            .pvcs
            .iter()
            .find(|pvc| pvc.name_any() == "db-2-data")
            .and_then(ResourceExt::uid)
            .unwrap(),
        old_pvc_uid
    );
}

#[tokio::test]
async fn cleanup_preserves_same_name_replacements_and_waits_on_lookup_failure() {
    let api = Arc::new(InMemoryClusterApi::new(fixture(1, 2)));
    drive_until_cleanup(&api).await;
    let cleanup = api
        .observation()
        .await
        .set
        .status
        .as_ref()
        .unwrap()
        .authority
        .scale_up_cleanup
        .as_deref()
        .unwrap()
        .clone();
    let endpoint_name = match &cleanup.resources.endpoint {
        kuberic_protocol::types::CleanupResourceIdentity::Present { name, .. }
        | kuberic_protocol::types::CleanupResourceIdentity::Absent { name } => name.clone(),
    };
    api.fail_exact_lookup(
        format!("Service/{endpoint_name}"),
        Some("temporary lookup failure".into()),
    )
    .await;
    let before = api.effects().await.len();
    tick(&api).await;
    assert!(
        api.effects().await[before..]
            .iter()
            .all(|effect| !matches!(effect, EffectRecord::DeleteScaleDownResource { .. }))
    );
    api.fail_exact_lookup(format!("Service/{endpoint_name}"), None)
        .await;

    let mut raw = api.observation().await;
    for pod in raw.pods.iter_mut().filter(|pod| pod.name_any() == "db-2") {
        pod.metadata.uid = Some("replacement-pod".into());
        pod.metadata.resource_version = Some("replacement-rv-pod".into());
    }
    for pvc in raw
        .pvcs
        .iter_mut()
        .filter(|pvc| pvc.name_any() == "db-2-data")
    {
        pvc.metadata.uid = Some("replacement-pvc".into());
        pvc.metadata.resource_version = Some("replacement-rv-pvc".into());
    }
    for service in raw
        .services
        .iter_mut()
        .filter(|service| service.name_any() == endpoint_name)
    {
        service.metadata.uid = Some("replacement-endpoint".into());
        service.metadata.resource_version = Some("replacement-rv-endpoint".into());
    }
    api.set_observation(raw).await;
    finish(&api, 1).await;
    let preserved = api.observation().await;
    assert!(
        preserved
            .pods
            .iter()
            .any(|pod| pod.uid().as_deref() == Some("replacement-pod"))
    );
    assert!(
        preserved
            .pvcs
            .iter()
            .any(|pvc| pvc.uid().as_deref() == Some("replacement-pvc"))
    );
    assert!(
        preserved
            .services
            .iter()
            .any(|service| { service.uid().as_deref() == Some("replacement-endpoint") })
    );
}

#[tokio::test]
async fn terminating_candidate_blocks_pvc_cleanup_until_exact_pod_absence() {
    let api = Arc::new(InMemoryClusterApi::new(fixture(1, 2)));
    drive_until_cleanup(&api).await;
    let mut raw = api.observation().await;
    raw.pods
        .iter_mut()
        .find(|pod| pod.name_any() == "db-2")
        .unwrap()
        .metadata
        .finalizers = Some(vec!["tests/finalizer".into()]);
    api.set_observation(raw).await;

    let mut saw_blocked_pod_delete = false;
    for _ in 0..10 {
        let (_, effects) = tick(&api).await;
        if effects.iter().any(|effect| {
            matches!(
                effect,
                EffectRecord::DeleteScaleDownResource {
                    resource: ScaleDownResource::Pod,
                    ..
                }
            )
        }) {
            saw_blocked_pod_delete = true;
            break;
        }
    }
    assert!(saw_blocked_pod_delete);
    let observed = api.observation().await;
    assert!(observed.pods.iter().any(|pod| pod.name_any() == "db-2"));
    assert!(
        observed
            .pvcs
            .iter()
            .any(|pvc| pvc.name_any() == "db-2-data")
    );
    assert!(
        observed
            .set
            .status
            .as_ref()
            .unwrap()
            .authority
            .scale_up_cleanup
            .is_some()
    );
    let (_, repeated) = tick(&api).await;
    assert!(repeated.iter().any(|effect| {
        matches!(
            effect,
            EffectRecord::DeleteScaleDownResource {
                resource: ScaleDownResource::Pod,
                ..
            }
        )
    }));
    assert!(repeated.iter().all(|effect| {
        !matches!(
            effect,
            EffectRecord::DeleteScaleDownResource {
                resource: ScaleDownResource::Pvc,
                ..
            }
        )
    }));
    let mut raw = api.observation().await;
    raw.pods
        .iter_mut()
        .find(|pod| pod.name_any() == "db-2")
        .unwrap()
        .metadata
        .finalizers = None;
    api.set_observation(raw).await;
    finish(&api, 1).await;
    let completed = api.observation().await;
    assert!(completed.pods.iter().all(|pod| pod.name_any() != "db-2"));
    assert!(
        completed
            .pvcs
            .iter()
            .all(|pvc| pvc.name_any() != "db-2-data")
    );
}

#[tokio::test]
async fn stale_cleanup_resource_version_is_reobserved_without_broad_deletion() {
    let api = Arc::new(InMemoryClusterApi::new(fixture(1, 2)));
    drive_until_cleanup(&api).await;
    let raw = api.observe("tests", "db").await.unwrap();
    let snapshot = normalize(raw.clone(), BTreeMap::new()).unwrap();
    let Plan::Apply { changes } = evaluate(&snapshot, &enabled()) else {
        panic!("expected exact cleanup delete")
    };
    let KubernetesChange::DeleteScaleDownResource {
        resource,
        name,
        uid,
        resource_version,
    } = changes[0].clone()
    else {
        panic!("expected exact cleanup delete")
    };
    let mut raced = api.observation().await;
    let object = match resource {
        ScaleDownResource::Endpoint => raced
            .services
            .iter_mut()
            .find(|object| object.name_any() == name)
            .unwrap()
            .meta_mut(),
        ScaleDownResource::Pod => raced
            .pods
            .iter_mut()
            .find(|object| object.name_any() == name)
            .unwrap()
            .meta_mut(),
        ScaleDownResource::Pvc => raced
            .pvcs
            .iter_mut()
            .find(|object| object.name_any() == name)
            .unwrap()
            .meta_mut(),
    };
    object.resource_version = Some("raced-resource-version".into());
    api.set_observation(raced).await;
    assert_eq!(
        api.delete_scale_down_resource(&raw, resource, &name, &uid, &resource_version)
            .await,
        Err(ControllerError::ObservationStale)
    );
    assert!(api.observation().await.services.iter().any(|service| {
        service.name_any() == name && service.uid().as_deref() == Some(uid.as_str())
    }));
}

#[tokio::test]
async fn pending_candidate_cleanup_allows_primary_failover_but_blocks_retry() {
    let api = Arc::new(InMemoryClusterApi::new(fixture(2, 3)));
    drive_until_cleanup(&api).await;
    let cleanup = api
        .observation()
        .await
        .set
        .status
        .as_ref()
        .unwrap()
        .authority
        .scale_up_cleanup
        .as_deref()
        .unwrap()
        .clone();
    let endpoint_name = match cleanup.resources.endpoint {
        kuberic_protocol::types::CleanupResourceIdentity::Present { name, .. }
        | kuberic_protocol::types::CleanupResourceIdentity::Absent { name } => name,
    };
    api.fail_exact_lookup(
        format!("Service/{endpoint_name}"),
        Some("candidate endpoint lookup unavailable".into()),
    )
    .await;
    let before = api.effects().await.len();
    let mut raw = api.observation().await;
    let primary = raw
        .set
        .status
        .as_ref()
        .unwrap()
        .authority
        .topology
        .as_ref()
        .unwrap()
        .configuration
        .members
        .iter()
        .find(|member| member.role == ReplicaRole::Primary)
        .unwrap()
        .identity
        .clone();
    let RawAgentObservation::Report(report) = raw
        .agents
        .get_mut(&ReplicaObservationKey::new(
            primary.replica_id,
            primary.instance_id,
        ))
        .unwrap()
    else {
        panic!("primary report")
    };
    report.reported_fault = proto::FaultType::Permanent as i32;
    report.healthy = false;
    raw.set.spec.replicas = 3;
    raw.set.metadata.generation = Some(raw.set.metadata.generation.unwrap_or_default() + 1);
    api.set_observation(raw).await;

    let mut saw_failover = false;
    for _ in 0..30 {
        let mut raw = api.observation().await;
        raw.now_unix_seconds += 11;
        api.set_observation(raw).await;
        let (_, effects) = tick(&api).await;
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, EffectRecord::EnsureScaffolding(_))),
            "cleanup obligation must block a fresh scale-up candidate"
        );
        if effects.iter().any(|effect| {
            matches!(
                effect,
                EffectRecord::Execute(ProtocolCommand::EnsureConfiguration(command))
                    if command.transition_kind == TransitionKind::Failover
            )
        }) {
            saw_failover = true;
            break;
        }
    }
    assert!(
        saw_failover,
        "primary failover must outrank candidate cleanup"
    );
    assert!(
        api.effects().await[before..]
            .iter()
            .all(|effect| { !matches!(effect, EffectRecord::DeleteScaleDownResource { .. }) })
    );
}

#[tokio::test]
async fn accepted_repair_and_explicit_switchover_outrank_scale_up() {
    let replacement = Arc::new(InMemoryClusterApi::new(fixture(2, 3)));
    let mut raw = replacement.observation().await;
    let secondary = raw
        .set
        .status
        .as_ref()
        .unwrap()
        .authority
        .topology
        .as_ref()
        .unwrap()
        .configuration
        .members
        .iter()
        .find(|member| member.role == ReplicaRole::ActiveSecondary)
        .unwrap()
        .identity
        .clone();
    let RawAgentObservation::Report(report) = raw
        .agents
        .get_mut(&ReplicaObservationKey::new(
            secondary.replica_id,
            secondary.instance_id,
        ))
        .unwrap()
    else {
        panic!("secondary report")
    };
    report.reported_fault = proto::FaultType::Permanent as i32;
    raw.now_unix_seconds += 20;
    replacement.set_observation(raw).await;
    for _ in 0..10 {
        tick(&replacement).await;
        if replacement
            .effects()
            .await
            .iter()
            .any(|effect| matches!(effect, EffectRecord::EnsureReplacement(_)))
        {
            break;
        }
    }
    assert!(
        replacement
            .effects()
            .await
            .iter()
            .any(|effect| matches!(effect, EffectRecord::EnsureReplacement(_)))
    );
    assert!(
        replacement
            .observation()
            .await
            .pvcs
            .iter()
            .all(|pvc| pvc.name_any() != "db-3-data")
    );

    let switchover = Arc::new(InMemoryClusterApi::new(fixture(2, 3)));
    let mut raw = switchover.observation().await;
    raw.set.spec.switchover = Some(PlannedSwitchoverRequestSpec {
        request_id: "scale-up-priority".into(),
        target_replica_id: 2,
    });
    raw.set.metadata.generation = Some(raw.set.metadata.generation.unwrap_or_default() + 1);
    switchover.set_observation(raw).await;
    for _ in 0..5 {
        let (_, effects) = tick(&switchover).await;
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, EffectRecord::EnsureScaffolding(_)))
        );
        if effects.iter().any(|effect| {
            matches!(
                effect,
                EffectRecord::Execute(ProtocolCommand::PrepareSwitchover(_))
            )
        }) {
            return;
        }
    }
    panic!("explicit switchover did not outrank scale-up");
}
