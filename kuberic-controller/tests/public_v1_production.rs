use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kube::ResourceExt;
use kuberic_controller::cluster_api::{
    EffectRecord, InMemoryClusterApi, PublicV1OperationApi, PublicV1Scope,
};
use kuberic_controller::crd::{INSTANCE_LABEL, KubericSet, KubericSetSpec};
use kuberic_controller::observation::{RawAgentObservation, RawObservation};
use kuberic_controller::reconciler::{ReconcileKind, Reconciler};
use kuberic_runtime::protocol::observation::ReplicaObservationKey;
use kuberic_runtime::protocol::public_operations::{
    PublicProgress, PublicProgressRole, PublicV1AgentReport, PublicV1Identity,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, PodUid, PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity, ReplicaStorageIncarnationContext,
    ResourceUid, StorageGeneration, derive_agent_generation, derive_bootstrap_history,
    derive_history_initialization_id, derive_initialization_id,
};

fn empty_observation(replicas: u32) -> RawObservation {
    let mut set = KubericSet::new(
        "sample",
        KubericSetSpec {
            replicas,
            image: "example".into(),
            failover_delay_seconds: 30,
            switchover: None,
        },
    );
    set.metadata.namespace = Some("default".into());
    set.metadata.uid = Some("resource-1".into());
    set.metadata.resource_version = Some("1".into());
    set.metadata.generation = Some(1);
    RawObservation {
        set,
        pods: Vec::new(),
        pvcs: Vec::new(),
        services: Vec::new(),
        secrets: Vec::new(),
        agents: BTreeMap::new(),
        failures: Vec::new(),
        now_unix_seconds: 0,
    }
}

fn report_for(observation: &RawObservation) -> PublicV1AgentReport {
    let resource_uid = ResourceUid::new(observation.set.uid().expect("set UID"));
    let generation = PublicProtocolGeneration::new(1);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
    let pod = observation
        .pods
        .iter()
        .find(|pod| pod.name_any() == "sample-1")
        .expect("bootstrap Pod");
    let pvc = observation
        .pvcs
        .iter()
        .find(|pvc| pvc.name_any() == "sample-1-data")
        .expect("bootstrap PVC");
    let pod_uid = PodUid::new(pod.uid().expect("bootstrap Pod UID"));
    let pvc_uid = PvcUid::new(pvc.uid().expect("bootstrap PVC UID"));
    let replica_id = ReplicaId::new(1);
    let initialization_id = derive_initialization_id(&resource_uid, replica_id, &pod_uid, &pvc_uid);
    let identity = ReplicaIdentity {
        replica_id,
        instance_id: ReplicaInstanceId::new(pod_uid.as_str()),
        agent_generation: derive_agent_generation(&initialization_id),
    };
    let report = PublicV1AgentReport {
        identity: PublicV1Identity::new(1),
        resource_uid,
        replica: identity.clone(),
        process_session_id: kuberic_runtime::protocol::types::ProcessSessionId::new("session-1"),
        replicator_creation_id: kuberic_runtime::protocol::types::ReplicatorCreationId::new(
            "creation-1",
        ),
        report_sequence: 1,
        role: ReplicaRole::None,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        history,
        storage_incarnation: ReplicaStorageIncarnationContext {
            replica: identity.clone(),
            storage: ReplicaStorageIdentity::Persistent { pod_uid, pvc_uid },
            storage_generation: StorageGeneration::new("storage-1"),
        },
        progress: PublicProgress {
            role: PublicProgressRole::Recovery,
            first: 0,
            last: 0,
        },
        service_location: None,
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    };
    report.validate().unwrap();
    report
}

fn resources_ready(observation: &RawObservation) -> bool {
    observation.pods.len() == 1
        && observation.pvcs.len() == 1
        && observation.services.len() == 3
        && observation.secrets.len() == 1
}

#[tokio::test]
async fn production_reconciler_bootstraps_then_stabilizes_on_public_v1_reports() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let api = Arc::new(InMemoryClusterApi::new(empty_observation(1)));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(30, 5, 30),
        );
        for _ in 0..8 {
            let action = reconciler.reconcile("default", "sample").await.unwrap();
            assert_eq!(action.kind, ReconcileKind::Executed);
            let observed = api.observation().await;
            assert!(
                observed
                    .agents
                    .values()
                    .all(|agent| !matches!(agent, RawAgentObservation::PublicV1Report(_))),
                "resource convergence must not preseed established Public V1 reports"
            );
            if resources_ready(&observed) {
                break;
            }
        }
        let observed = api.observation().await;
        assert!(resources_ready(&observed));
        assert!(
            observed
                .agents
                .values()
                .any(|agent| matches!(agent, RawAgentObservation::Uninitialized(_)))
        );
        let pod_uid = observed.pods[0].uid().expect("bootstrap Pod UID");
        assert_eq!(
            observed.pods[0].labels().get(INSTANCE_LABEL),
            Some(&pod_uid)
        );
        api.set_observation(observed).await;
        let mut applied = false;
        for _ in 0..32 {
            match reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
            {
                ReconcileKind::Applied | ReconcileKind::Executed => applied = true,
                ReconcileKind::Stable => break,
                ReconcileKind::Waiting => {}
                other => panic!("unexpected production reconcile result: {other:?}"),
            }
        }
        assert!(applied);
        let observed = api.observation().await;
        let status = observed.set.status.as_ref().expect("Public V1 status");
        let topology = status
            .public_v1_topology
            .as_ref()
            .expect("accepted Public V1 topology")
            .configuration()
            .unwrap();
        assert_eq!(topology.members.len(), 1);
        assert_eq!(topology.primary_id, ReplicaId::new(1));
        let report = observed
            .agents
            .values()
            .find_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report) => Some(report),
                _ => None,
            })
            .expect("established Public V1 report");
        assert_eq!(report.role, ReplicaRole::Primary);
        assert_eq!(report.write_status, AccessStatus::Granted);
        let selector = observed
            .services
            .iter()
            .find(|service| service.metadata.name.as_deref() == Some("sample-write"))
            .and_then(|service| service.spec.as_ref())
            .and_then(|spec| spec.selector.as_ref())
            .and_then(|selector| selector.get(INSTANCE_LABEL));
        assert_eq!(selector, Some(&pod_uid));
    })
    .await
    .expect("public-v1 production reconciliation exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_defers_preseeded_reports_until_resources_exist() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let source_api = Arc::new(InMemoryClusterApi::new(empty_observation(1)));
        let source_reconciler = Reconciler::new(
            source_api.clone(),
            kuberic_controller::production_evaluation_config(30, 5, 30),
        );
        for _ in 0..8 {
            assert_eq!(
                source_reconciler
                    .reconcile("default", "sample")
                    .await
                    .unwrap()
                    .kind,
                ReconcileKind::Executed
            );
            if resources_ready(&source_api.observation().await) {
                break;
            }
        }
        let report = report_for(&source_api.observation().await);
        let mut preseeded = empty_observation(1);
        preseeded.agents.insert(
            ReplicaObservationKey::new(
                report.replica.replica_id,
                report.replica.instance_id.clone(),
            ),
            RawAgentObservation::PublicV1Report(Box::new(report)),
        );

        let api = Arc::new(InMemoryClusterApi::new(preseeded));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(30, 5, 30),
        );
        for _ in 0..8 {
            assert_eq!(
                reconciler
                    .reconcile("default", "sample")
                    .await
                    .unwrap()
                    .kind,
                ReconcileKind::Executed,
                "preseeded evidence was consumed before resource convergence"
            );
            let effects = api.effects().await;
            assert!(
                effects.iter().all(|effect| !matches!(
                    effect,
                    EffectRecord::ReplaceStatus | EffectRecord::Execute(_)
                )),
                "preseeded evidence persisted or dispatched before resource convergence"
            );
            let observed = api.observation().await;
            assert_eq!(observed.agents.len(), 1);
            if resources_ready(&observed) {
                break;
            }
        }
        assert!(resources_ready(&api.observation().await));
        assert!(
            api.effects()
                .await
                .iter()
                .any(|effect| matches!(effect, EffectRecord::EnsureScaffolding(_)))
        );
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
    })
    .await
    .expect("preseeded-report bootstrap reconciliation exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_initializes_every_desired_replica_and_one_writer() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let api = Arc::new(InMemoryClusterApi::new(empty_observation(3)));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(30, 5, 30),
        );
        let mut stable = false;
        for _ in 0..160 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                stable = true;
                break;
            }
        }
        assert!(stable, "three-replica Public V1 bootstrap did not converge");
        let observed = api.observation().await;
        assert_eq!(observed.pods.len(), 3);
        assert_eq!(observed.pvcs.len(), 3);
        let reports = observed
            .agents
            .values()
            .filter_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report) => Some(report.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reports.len(), 3);
        assert_eq!(
            reports
                .iter()
                .filter(|report| report.write_status == AccessStatus::Granted)
                .count(),
            1
        );
        assert_eq!(
            reports
                .iter()
                .find(|report| report.write_status == AccessStatus::Granted)
                .map(|report| report.replica.replica_id),
            Some(ReplicaId::new(1))
        );
        let topology = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .expect("accepted topology")
            .configuration()
            .unwrap();
        assert_eq!(topology.members.len(), 3);
    })
    .await
    .expect("three-replica Public V1 bootstrap exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_originates_failover_and_retargets_writer() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let api = Arc::new(InMemoryClusterApi::new(empty_observation(3)));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(0, 1, 1),
        );
        for _ in 0..160 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                break;
            }
        }
        let mut observed = api.observation().await;
        let primary_key = observed
            .agents
            .iter()
            .find_map(|(key, agent)| match agent {
                RawAgentObservation::PublicV1Report(report)
                    if report.write_status == AccessStatus::Granted =>
                {
                    Some(key.clone())
                }
                _ => None,
            })
            .expect("initial primary");
        assert_eq!(primary_key.replica_id, ReplicaId::new(1));
        observed.agents.remove(&primary_key);
        api.set_observation(observed).await;
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Waiting
        );
        let mut observed = api.observation().await;
        observed.now_unix_seconds = 30;
        api.set_observation(observed).await;

        let mut stable = false;
        for _ in 0..160 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                stable = true;
                break;
            }
        }
        assert!(stable, "Public V1 failover did not converge");
        let observed = api.observation().await;
        let reports = observed
            .agents
            .values()
            .filter_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report) => Some(report.as_ref()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(reports.len(), 2);
        let writer = reports
            .iter()
            .filter(|report| report.write_status == AccessStatus::Granted)
            .collect::<Vec<_>>();
        assert_eq!(writer.len(), 1);
        assert_ne!(writer[0].replica.replica_id, ReplicaId::new(1));
        let topology = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .expect("failover topology")
            .configuration()
            .unwrap();
        assert_eq!(topology.members.len(), 2);
        assert_eq!(topology.primary_id, writer[0].replica.replica_id);
        let selector = observed
            .services
            .iter()
            .find(|service| service.metadata.name.as_deref() == Some("sample-write"))
            .and_then(|service| service.spec.as_ref())
            .and_then(|spec| spec.selector.as_ref())
            .and_then(|selector| selector.get(INSTANCE_LABEL));
        assert_eq!(selector, Some(&writer[0].replica.instance_id.to_string()));
    })
    .await
    .expect("Public V1 failover exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_replaces_admitted_replica_with_fresh_storage() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let api = Arc::new(InMemoryClusterApi::new(empty_observation(3)));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(0, 1, 1),
        );
        for _ in 0..160 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                break;
            }
        }
        let mut observed = api.observation().await;
        let old = observed
            .agents
            .iter()
            .find_map(|(key, agent)| match agent {
                RawAgentObservation::PublicV1Report(report)
                    if report.replica.replica_id == ReplicaId::new(2) =>
                {
                    Some((key.clone(), report.replica.clone()))
                }
                _ => None,
            })
            .expect("admitted secondary");
        observed.agents.remove(&old.0);
        observed
            .pods
            .retain(|pod| pod.uid().as_deref() != Some(old.1.instance_id.as_str()));
        api.set_observation(observed).await;
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        let observed = api.observation().await;
        let replacement_authority = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_replacement.as_ref())
            .expect("replacement authority persisted before allocation");
        assert_eq!(replacement_authority.predecessor.replica, old.1);
        assert!(replacement_authority.replacement.is_none());
        assert!(
            observed
                .pvcs
                .iter()
                .any(|pvc| pvc.metadata.name.as_deref() == Some("sample-2-data"))
        );
        let authority_operation = replacement_authority.operation_id.clone();
        let authority_predecessor = replacement_authority.predecessor.clone();
        let scope = PublicV1Scope {
            namespace: "default".into(),
            name: "sample".into(),
            resource_uid: ResourceUid::new("resource-1"),
        };
        let effects_start = api.effects().await.len();
        let mut bound_target = None;
        let mut build_seen = false;
        let mut admitted_with_authority = false;
        let mut predecessor_cleaned_with_authority = false;
        let mut rebind_rejected = false;

        let mut stable = false;
        for iteration in 0..200 {
            let action = match reconciler.reconcile("default", "sample").await {
                Ok(action) => action,
                Err(error) => panic!(
                    "replacement iteration {iteration}: {error:?}; observation={:#?}",
                    api.observation().await
                ),
            };
            let observed = api.observation().await;
            let authority = observed
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_replacement.as_ref());
            if let Some(authority) = authority {
                assert_eq!(authority.operation_id, authority_operation);
                assert_eq!(authority.predecessor, authority_predecessor);
                if let Some(target) = authority.replacement.as_ref() {
                    if let Some(bound) = &bound_target {
                        assert_eq!(target, bound);
                    } else {
                        bound_target = Some(target.clone());
                    }
                    if !rebind_rejected {
                        assert!(
                            api.bind_public_v1_replacement(
                                &scope,
                                &authority_operation,
                                target,
                            )
                            .await
                            .is_err()
                        );
                        rebind_rejected = true;
                    }
                }
            }
            let effects = api.effects().await;
            let build_dispatched = effects[effects_start..].iter().any(|effect| {
                matches!(
                    effect,
                    EffectRecord::Execute(
                        kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(
                            command
                        )
                    ) if matches!(
                        command.intent.program,
                        Some(
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Build(_)
                        )
                    )
                )
            });
            if build_dispatched && !build_seen {
                build_seen = true;
                let authority = authority.expect("Build retains replacement authority");
                assert_eq!(authority.replacement.as_ref(), bound_target.as_ref());
            }
            if let Some(target) = bound_target.as_ref() {
                let admitted = observed
                    .set
                    .status
                    .as_ref()
                    .and_then(|status| status.public_v1_topology.as_ref())
                    .and_then(|topology| topology.configuration().ok())
                    .is_some_and(|configuration| {
                        configuration
                            .members
                            .iter()
                            .any(|member| member.identity == target.replica)
                    });
                let predecessor_present = observed
                    .pvcs
                    .iter()
                    .any(|pvc| pvc.metadata.name.as_deref() == Some("sample-2-data"));
                if admitted && predecessor_present {
                    assert!(authority.is_some());
                    admitted_with_authority = true;
                }
                if admitted && !predecessor_present && authority.is_some() {
                    predecessor_cleaned_with_authority = true;
                }
            }
            if action.kind == ReconcileKind::Stable {
                stable = true;
                break;
            }
        }
        assert!(stable, "Public V1 replacement did not converge");
        assert!(rebind_rejected);
        assert!(build_seen);
        assert!(admitted_with_authority);
        assert!(predecessor_cleaned_with_authority);
        let observed = api.observation().await;
        let replacement = observed
            .agents
            .values()
            .find_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report)
                    if report.replica.replica_id == ReplicaId::new(2) =>
                {
                    Some(report.as_ref())
                }
                _ => None,
            })
            .expect("replacement report");
        assert_ne!(replacement.replica, old.1);
        assert!(observed.pvcs.iter().any(|pvc| {
            pvc.labels()
                .get(kuberic_controller::crd::REPLICA_ID_LABEL)
                .is_some_and(|value| value == "2")
                && pvc.metadata.name.as_deref() != Some("sample-2-data")
        }));
        assert!(
            !observed
                .pvcs
                .iter()
                .any(|pvc| pvc.metadata.name.as_deref() == Some("sample-2-data"))
        );
        let topology = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .expect("replacement topology")
            .configuration()
            .unwrap();
        assert!(
            topology
                .members
                .iter()
                .any(|member| member.identity == replacement.replica)
        );
        assert_eq!(
            observed
                .agents
                .values()
                .filter(|agent| matches!(
                    agent,
                    RawAgentObservation::PublicV1Report(report)
                        if report.write_status == AccessStatus::Granted
                ))
                .count(),
            1
        );
        assert!(
            observed
                .set
                .status
                .as_ref()
                .is_some_and(|status| status.public_v1_replacement.is_none())
        );
    })
    .await
    .expect("Public V1 replacement exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_originates_planned_switchover() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let api = Arc::new(InMemoryClusterApi::new(empty_observation(3)));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(0, 1, 1),
        );
        for _ in 0..160 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                break;
            }
        }
        let mut observed = api.observation().await;
        observed.set.spec.switchover =
            Some(kuberic_controller::crd::PlannedSwitchoverRequestSpec {
                request_id: "switch-to-2".into(),
                target_replica_id: 2,
            });
        api.set_observation(observed).await;
        let mut stable = false;
        for _ in 0..160 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                stable = true;
                break;
            }
        }
        assert!(stable);
        let observed = api.observation().await;
        let writers = observed
            .agents
            .values()
            .filter_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report)
                    if report.write_status == AccessStatus::Granted =>
                {
                    Some(report.replica.replica_id)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(writers, vec![ReplicaId::new(2)]);
    })
    .await
    .expect("Public V1 switchover exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_originates_desired_membership_changes() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let api = Arc::new(InMemoryClusterApi::new(empty_observation(2)));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(0, 1, 1),
        );
        for _ in 0..160 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                break;
            }
        }
        let mut observed = api.observation().await;
        observed.set.spec.replicas = 3;
        api.set_observation(observed).await;
        let mut scaled_up = false;
        for _ in 0..200 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                scaled_up = true;
                break;
            }
        }
        assert!(scaled_up);
        let observed = api.observation().await;
        assert_eq!(
            observed
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_topology.as_ref())
                .unwrap()
                .configuration()
                .unwrap()
                .members
                .len(),
            3
        );

        let mut observed = observed;
        observed.set.spec.replicas = 2;
        api.set_observation(observed).await;
        let mut scaled_down = false;
        for _ in 0..200 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                scaled_down = true;
                break;
            }
        }
        if !scaled_down {
            panic!(
                "scale down did not converge: observation={:#?}",
                api.observation().await
            );
        }
        let observed = api.observation().await;
        let topology = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .unwrap()
            .configuration()
            .unwrap();
        assert_eq!(topology.members.len(), 2);
        assert!(!observed.pods.iter().any(|pod| {
            pod.labels()
                .get(kuberic_controller::crd::REPLICA_ID_LABEL)
                .is_some_and(|value| value == "3")
        }));
        assert_eq!(
            observed
                .agents
                .values()
                .filter(|agent| matches!(
                    agent,
                    RawAgentObservation::PublicV1Report(report)
                        if report.write_status == AccessStatus::Granted
                ))
                .count(),
            1
        );
    })
    .await
    .expect("Public V1 desired-membership change exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_transfers_routing_before_removing_current_primary() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let api = Arc::new(InMemoryClusterApi::new(empty_observation(3)));
        let mut reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(0, 1, 1),
        );
        for _ in 0..160 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                break;
            }
        }
        let mut observed = api.observation().await;
        observed.set.spec.switchover =
            Some(kuberic_controller::crd::PlannedSwitchoverRequestSpec {
                request_id: "make-three-primary".into(),
                target_replica_id: 3,
            });
        api.set_observation(observed).await;
        for _ in 0..160 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                break;
            }
        }
        let mut observed = api.observation().await;
        observed.set.spec.switchover = None;
        observed.set.spec.replicas = 2;
        api.set_observation(observed).await;
        let start = api.effects().await.len();
        let mut restarted_after_handoff = false;
        let mut remove_seen = false;
        for _ in 0..240 {
            reconciler.reconcile("default", "sample").await.unwrap();
            let effects = api.effects().await;
            let new_effects = &effects[start..];
            if !restarted_after_handoff {
                let observed = api.observation().await;
                let handoff = observed
                    .set
                    .status
                    .as_ref()
                    .and_then(|status| status.public_v1_topology.as_ref())
                    .and_then(|topology| topology.configuration().ok())
                    .filter(|configuration| {
                        configuration.members.len() == 3
                            && configuration.primary_id != ReplicaId::new(3)
                            && configuration.members.iter().any(|member| {
                                member.identity.replica_id == ReplicaId::new(3)
                                    && member.role == ReplicaRole::ActiveSecondary
                            })
                    });
                if handoff.is_some() {
                    assert_eq!(
                        observed
                            .agents
                            .values()
                            .filter(|agent| matches!(
                                agent,
                                RawAgentObservation::PublicV1Report(report)
                                    if report.write_status == AccessStatus::Granted
                            ))
                            .count(),
                        1
                    );
                    reconciler = Reconciler::new(
                        api.clone(),
                        kuberic_controller::production_evaluation_config(0, 1, 1),
                    );
                    restarted_after_handoff = true;
                }
            }
            if new_effects.iter().any(|effect| {
                matches!(
                    effect,
                    EffectRecord::Execute(
                        kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(
                            command
                        )
                    ) if matches!(
                        command.intent.program,
                        Some(
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Remove(_)
                        )
                    )
                )
            }) {
                remove_seen = true;
                let observed = api.observation().await;
                let successor = observed
                    .agents
                    .values()
                    .find_map(|agent| match agent {
                        RawAgentObservation::PublicV1Report(report)
                            if report.write_status == AccessStatus::Granted =>
                        {
                            Some(report.replica.clone())
                        }
                        _ => None,
                    })
                    .expect("accepted successor writer");
                assert_ne!(successor.replica_id, ReplicaId::new(3));
                let selector = observed
                    .services
                    .iter()
                    .find(|service| service.name_any() == "sample-write")
                    .and_then(|service| service.spec.as_ref())
                    .and_then(|spec| spec.selector.as_ref())
                    .and_then(|selector| selector.get(INSTANCE_LABEL));
                assert_eq!(selector, Some(&successor.instance_id.to_string()));
                assert_eq!(
                    observed
                        .agents
                        .values()
                        .filter(|agent| matches!(
                            agent,
                            RawAgentObservation::PublicV1Report(report)
                                if report.write_status == AccessStatus::Granted
                        ))
                        .count(),
                    1
                );
                break;
            }
        }
        let effects = api.effects().await;
        assert!(restarted_after_handoff);
        assert!(remove_seen);
        let new_effects = &effects[start..];
        let catch_up = new_effects
            .iter()
            .position(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if matches!(
                    command.intent.program,
                    Some(
                        kuberic_runtime::protocol::public_operations::PublicOperationProgram::CatchUp { .. }
                    )
                )
            ))
            .expect("CatchUp effect");
        let swap = new_effects
            .iter()
            .position(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if matches!(
                    command.intent.program,
                    Some(
                        kuberic_runtime::protocol::public_operations::PublicOperationProgram::Swap { .. }
                    )
                )
            ))
            .expect("Swap effect");
        let route = new_effects
            .iter()
            .position(|effect| matches!(effect, EffectRecord::RoutePrimary(_)))
            .expect("route effect");
        let remove = new_effects
            .iter()
            .position(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if matches!(
                    command.intent.program,
                    Some(
                        kuberic_runtime::protocol::public_operations::PublicOperationProgram::Remove(_)
                    )
                )
            ))
            .expect("Remove effect");
        assert!(catch_up < swap && swap < route && route < remove);
    })
    .await
    .expect("Public V1 primary removal ordering exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_originates_and_completes_session_renewal() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let api = Arc::new(InMemoryClusterApi::new(empty_observation(3)));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(0, 1, 1),
        );
        for _ in 0..160 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                break;
            }
        }
        let mut observed = api.observation().await;
        let stable_observation = observed.clone();
        let previous_epoch = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .expect("initial topology")
            .epoch;
        let (key, mut restarted) = observed
            .agents
            .iter()
            .find_map(|(key, agent)| match agent {
                RawAgentObservation::PublicV1Report(report)
                    if report.write_status == AccessStatus::Granted =>
                {
                    Some((key.clone(), report.as_ref().clone()))
                }
                _ => None,
            })
            .expect("current primary report");
        let original_report = restarted.clone();
        let effects_before = api.effects().await.len();
        let mut session_only = original_report.clone();
        session_only.process_session_id =
            kuberic_runtime::protocol::types::ProcessSessionId::new("session-only-change");
        observed.agents.insert(
            key.clone(),
            RawAgentObservation::PublicV1Report(Box::new(session_only)),
        );
        api.set_observation(observed).await;
        assert!(reconciler.reconcile("default", "sample").await.is_err());
        assert_eq!(api.effects().await.len(), effects_before);

        let mut creation_only_observation = stable_observation.clone();
        let mut creation_only = original_report.clone();
        creation_only.replicator_creation_id =
            kuberic_runtime::protocol::types::ReplicatorCreationId::new("creation-only-change");
        creation_only_observation.agents.insert(
            key.clone(),
            RawAgentObservation::PublicV1Report(Box::new(creation_only)),
        );
        api.set_observation(creation_only_observation).await;
        assert!(reconciler.reconcile("default", "sample").await.is_err());
        assert_eq!(api.effects().await.len(), effects_before);

        restarted.process_session_id =
            kuberic_runtime::protocol::types::ProcessSessionId::new("restarted-primary-session");
        restarted.replicator_creation_id =
            kuberic_runtime::protocol::types::ReplicatorCreationId::new(
                "restarted-primary-creation",
            );
        restarted.report_sequence = 1;
        restarted.role = ReplicaRole::None;
        restarted.read_status = AccessStatus::NotPrimary;
        restarted.write_status = AccessStatus::NotPrimary;
        restarted.progress.role = PublicProgressRole::Recovery;
        restarted.service_location = None;
        restarted.completed_operations.clear();
        let mut valid_restart_observation = stable_observation;
        valid_restart_observation.agents.insert(
            key.clone(),
            RawAgentObservation::PublicV1Report(Box::new(restarted.clone())),
        );
        api.set_observation(valid_restart_observation).await;
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        let accepted_renewal = api
            .observation()
            .await
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_session_renewal.as_ref())
            .expect("accepted renewal")
            .renewal()
            .unwrap();
        let mut replaced_again_observation = api.observation().await;
        let mut replaced_again = restarted;
        replaced_again.process_session_id =
            kuberic_runtime::protocol::types::ProcessSessionId::new("second-restart-session");
        replaced_again.replicator_creation_id =
            kuberic_runtime::protocol::types::ReplicatorCreationId::new("second-restart-creation");
        replaced_again_observation.agents.insert(
            key.clone(),
            RawAgentObservation::PublicV1Report(Box::new(replaced_again)),
        );
        api.set_observation(replaced_again_observation).await;
        assert!(reconciler.reconcile("default", "sample").await.is_err());
        assert_eq!(
            api.observation()
                .await
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_session_renewal.as_ref())
                .unwrap()
                .renewal()
                .unwrap(),
            accepted_renewal
        );
        let mut restored = api.observation().await;
        restored.agents.insert(
            key,
            RawAgentObservation::PublicV1Report(Box::new(
                accepted_renewal
                    .required
                    .replacement
                    .as_ref()
                    .map(|replacement| {
                        let mut report = original_report;
                        report.process_session_id = replacement.process_session_id.clone();
                        report.replicator_creation_id = replacement.replicator_creation_id.clone();
                        report.report_sequence = 1;
                        report.role = ReplicaRole::None;
                        report.read_status = AccessStatus::NotPrimary;
                        report.write_status = AccessStatus::NotPrimary;
                        report.progress.role = PublicProgressRole::Recovery;
                        report.service_location = None;
                        report.completed_operations.clear();
                        report
                    })
                    .unwrap(),
            )),
        );
        api.set_observation(restored).await;

        let mut stable = false;
        for _ in 0..120 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                stable = true;
                break;
            }
        }
        if !stable {
            panic!(
                "session renewal replay did not converge: {:#?}",
                api.observation().await
            );
        }
        let observed = api.observation().await;
        let writers = observed
            .agents
            .values()
            .filter(|agent| {
                matches!(
                    agent,
                    RawAgentObservation::PublicV1Report(report)
                        if report.write_status == AccessStatus::Granted
                )
            })
            .count();
        assert_eq!(writers, 1);
        let topology = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .expect("renewed topology");
        assert!(topology.epoch > previous_epoch);
        assert!(topology.sessions.iter().any(|session| {
            session.process_session_id.as_str() == "restarted-primary-session"
                && session.replicator_creation_id.as_str() == "restarted-primary-creation"
        }));
        assert!(
            observed
                .set
                .status
                .as_ref()
                .is_some_and(|status| status.public_v1_session_renewal.is_none())
        );
        assert!(observed.agents.values().any(|agent| matches!(
            agent,
            RawAgentObservation::PublicV1Report(report)
                if report.process_session_id.as_str() == "restarted-primary-session"
                    && report.role == ReplicaRole::Primary
                    && report.write_status == AccessStatus::Granted
        )));
    })
    .await
    .expect("Public V1 session replay exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_consumes_completion_after_lost_dispatch_reply() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let api = Arc::new(InMemoryClusterApi::new(empty_observation(1)));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(0, 1, 1),
        );
        for _ in 0..80 {
            reconciler.reconcile("default", "sample").await.unwrap();
            if api
                .observation()
                .await
                .set
                .status
                .as_ref()
                .is_some_and(|status| status.public_v1_transition.is_some())
            {
                break;
            }
        }
        assert!(
            api.observation()
                .await
                .set
                .status
                .as_ref()
                .is_some_and(|status| status.public_v1_transition.is_some())
        );
        api.unavailable_next_execute().await;
        assert!(
            reconciler.reconcile("default", "sample").await.is_ok(),
            "durable matching completion must resolve an ambiguous dispatch reply"
        );
        let mut stable = false;
        for _ in 0..80 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                stable = true;
                break;
            }
        }
        assert!(stable);
    })
    .await
    .expect("ambiguous dispatch reconciliation exceeded 270 seconds");
}
