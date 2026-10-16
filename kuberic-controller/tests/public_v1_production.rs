use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use kube::ResourceExt;
use kuberic_controller::cluster_api::{
    EffectRecord, InMemoryClusterApi, PublicV1OperationApi, PublicV1Scope,
};
use kuberic_controller::crd::{
    INSTANCE_LABEL, KubericSet, KubericSetSpec, PlannedSwitchoverRequestSpec,
};
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
        replication_address: None,
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
        let initial_topology = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .expect("initial topology")
            .configuration()
            .unwrap();
        let initial_route = observed
            .services
            .iter()
            .find(|service| service.metadata.name.as_deref() == Some("sample-write"))
            .and_then(|service| service.spec.as_ref())
            .and_then(|spec| spec.selector.as_ref())
            .and_then(|selector| selector.get(INSTANCE_LABEL))
            .cloned();
        let primary_report = observed
            .agents
            .remove(&primary_key)
            .expect("removed primary report");
        api.set_observation(observed).await;
        let effects_start = api.effects().await.len();
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
        observed
            .agents
            .insert(primary_key.clone(), primary_report.clone());
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
        let topology = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .expect("recovered topology");
        assert!(topology.unavailable_primary_id.is_none());
        assert_eq!(topology.configuration().unwrap(), initial_topology);
        assert_eq!(
            observed
                .services
                .iter()
                .find(|service| service.metadata.name.as_deref() == Some("sample-write"))
                .and_then(|service| service.spec.as_ref())
                .and_then(|spec| spec.selector.as_ref())
                .and_then(|selector| selector.get(INSTANCE_LABEL))
                .cloned(),
            initial_route
        );
        assert!(api.effects().await[effects_start..].iter().all(|effect| {
            !matches!(effect, EffectRecord::Execute(
                kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(_)
            ))
        }));

        let mut observed = api.observation().await;
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
        let effects = api.effects().await;
        let failover_effects = &effects[effects_start..];
        let authority_installations = failover_effects
            .iter()
            .filter_map(|effect| match effect {
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command),
                ) => match command.intent.program.as_ref() {
                    Some(
                        kuberic_runtime::protocol::public_operations::PublicOperationProgram::Configuration(
                            _
                        )
                    ) => Some((command.target.replica_id, false)),
                    Some(
                        kuberic_runtime::protocol::public_operations::PublicOperationProgram::CatchUp {
                            ..
                        }
                    ) => Some((command.target.replica_id, true)),
                    _ => None,
                },
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            authority_installations.last(),
            Some(&(writer[0].replica.replica_id, true)),
            "failover must configure the new primary after its surviving secondaries"
        );
        assert!(
            authority_installations[..authority_installations.len() - 1]
                .iter()
                .all(|(replica_id, _)| *replica_id != writer[0].replica.replica_id),
            "failover configured the new primary before a surviving secondary"
        );
        let open_operations = failover_effects
            .iter()
            .filter_map(|effect| match effect {
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command),
                ) if matches!(
                    command.intent.program,
                    Some(
                        kuberic_runtime::protocol::public_operations::PublicOperationProgram::Open {
                            ..
                        }
                    )
                ) =>
                {
                    Some(command.intent.operation_id.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(
            open_operations.is_empty(),
            "failover must not reopen retained replicas: {open_operations:?}"
        );
    })
    .await
    .expect("Public V1 failover exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_fails_over_before_replacing_deleted_primary_pod() {
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
        let primary = observed
            .agents
            .iter()
            .find_map(|(key, agent)| match agent {
                RawAgentObservation::PublicV1Report(report)
                    if report.write_status == AccessStatus::Granted =>
                {
                    Some((key.clone(), report.replica.clone()))
                }
                _ => None,
            })
            .expect("initial primary");
        observed.agents.remove(&primary.0);
        observed
            .pods
            .retain(|pod| pod.uid().as_deref() != Some(primary.1.instance_id.as_str()));
        api.set_observation(observed).await;
        let effects_start = api.effects().await.len();

        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        let mut observed = api.observation().await;
        observed.now_unix_seconds = 30;
        api.set_observation(observed).await;

        let mut converged = false;
        for _ in 0..300 {
            let action = reconciler.reconcile("default", "sample").await.unwrap();
            let observed = api.observation().await;
            let complete = observed.set.status.as_ref().is_some_and(|status| {
                status.public_v1_replacement.is_none()
                    && status.public_v1_transition.is_none()
                    && status
                        .public_v1_topology
                        .as_ref()
                        .and_then(|topology| topology.configuration().ok())
                        .is_some_and(|configuration| configuration.members.len() == 3)
            });
            if action.kind == ReconcileKind::Stable && complete {
                converged = true;
                break;
            }
        }
        assert!(
            converged,
            "deleted-primary failover and replacement did not converge: observation={:#?} effects={:#?}",
            api.observation().await,
            api.effects().await
        );
        let observed = api.observation().await;
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
        let replacement = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .and_then(|topology| {
                topology
                    .sessions
                    .iter()
                    .find(|session| session.identity.replica_id == primary.1.replica_id)
            })
            .expect("replacement primary session");
        assert_ne!(replacement.identity, primary.1);
        let effects = api.effects().await;
        let route = effects[effects_start..]
            .iter()
            .position(|effect| matches!(effect, EffectRecord::RoutePrimary(_)))
            .expect("survivor route");
        let replacement_start = effects[effects_start..]
            .iter()
            .position(|effect| {
                matches!(
                    effect,
                    EffectRecord::EnsureReplacement(identity)
                        if identity.replica_id == primary.1.replica_id
                )
            })
            .expect("replacement scaffolding");
        assert!(route < replacement_start);
    })
    .await
    .expect("deleted-primary recovery exceeded 270 seconds");
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
        let predecessor_epoch = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .expect("predecessor topology")
            .configuration()
            .unwrap()
            .epoch;
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
        let predecessor_report = observed
            .agents
            .get(&old.0)
            .cloned()
            .expect("predecessor report");
        observed.agents.remove(&old.0);
        observed
            .pods
            .iter_mut()
            .find(|pod| pod.uid().as_deref() == Some(old.1.instance_id.as_str()))
            .expect("admitted secondary Pod")
            .metadata
            .deletion_timestamp = Some(
            serde_json::from_str("\"2026-01-01T00:00:00Z\"").unwrap(),
        );
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
        let mut observed = observed;
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
            ReconcileKind::Executed
        );
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Executed
        );
        let mut observed = api.observation().await;
        let replacement_agent = observed
            .agents
            .iter()
            .find_map(|(key, agent)| {
                (key.replica_id == ReplicaId::new(2)
                    && matches!(agent, RawAgentObservation::Uninitialized(_)))
                .then(|| (key.clone(), agent.clone()))
            })
            .expect("fresh replacement agent");
        observed.agents.remove(&replacement_agent.0);
        api.set_observation(observed).await;
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Executed
        );
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Waiting,
            "pending replacement must preserve the admitted topology while its agent is unavailable"
        );
        let mut observed = api.observation().await;
        let status = observed.set.status.as_ref().expect("status");
        assert!(status.public_v1_transition.is_none());
        assert!(status.public_v1_operation.is_none());
        assert_eq!(
            status
                .public_v1_topology
                .as_ref()
                .expect("topology")
                .configuration()
                .unwrap()
                .epoch,
            predecessor_epoch
        );
        observed
            .agents
            .insert(replacement_agent.0, replacement_agent.1);
        let primary_key = observed
            .agents
            .iter()
            .find_map(|(key, agent)| match agent {
                RawAgentObservation::PublicV1Report(report)
                    if report.role == ReplicaRole::Primary =>
                {
                    Some(key.clone())
                }
                _ => None,
            })
            .expect("retained primary report");
        let RawAgentObservation::PublicV1Report(primary) =
            observed.agents.get_mut(&primary_key).expect("primary report")
        else {
            unreachable!()
        };
        primary.role = ReplicaRole::None;
        primary.read_status = AccessStatus::NotPrimary;
        primary.write_status = AccessStatus::NotPrimary;
        primary.progress.role = PublicProgressRole::Recovery;
        let primary_service_location = primary.service_location.take();
        api.set_observation(observed).await;
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
        let mut source_waited_before_build = false;
        let mut overlap_injected = false;

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
                        let mut overlapping = api.observation().await;
                        overlapping
                            .agents
                            .insert(old.0.clone(), predecessor_report.clone());
                        api.set_observation(overlapping).await;
                        overlap_injected = true;
                        continue;
                    }
                    if !rebind_rejected {
                        let before = authority.clone();
                        let mut conflicting = target.clone();
                        conflicting.replica.instance_id =
                            ReplicaInstanceId::new("conflicting-replacement-pod");
                        conflicting.replica.agent_generation =
                            kuberic_runtime::protocol::types::AgentGeneration::new(
                                "conflicting-replacement-generation",
                            );
                        conflicting.storage =
                            ReplicaStorageIdentity::Persistent {
                                pod_uid: PodUid::new("conflicting-replacement-pod"),
                                pvc_uid: PvcUid::new("conflicting-replacement-pvc"),
                            };
                        conflicting.storage_generation =
                            StorageGeneration::new("conflicting-replacement-storage");
                        assert!(
                            api.bind_public_v1_replacement(
                                &scope,
                                &authority_operation,
                                &conflicting,
                            )
                            .await
                            .is_err()
                        );
                        assert_eq!(
                            api.observation()
                                .await
                                .set
                                .status
                                .as_ref()
                                .and_then(|status| status.public_v1_replacement.as_ref())
                                .unwrap(),
                            &before
                        );
                        rebind_rejected = true;
                    }
                }
            }
            let effects = api.effects().await;
            let replacement_open_dispatched = bound_target.as_ref().is_some_and(|target| {
                effects[effects_start..].iter().any(|effect| {
                    matches!(
                        effect,
                        EffectRecord::Execute(
                            kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(
                                command
                            )
                        ) if command.target == target.replica
                            && matches!(
                                command.intent.program,
                                Some(
                                    kuberic_runtime::protocol::public_operations::PublicOperationProgram::Open {
                                        ..
                                    }
                                )
                            )
                    )
                })
            });
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
            if !source_waited_before_build
                && replacement_open_dispatched
                && action.kind == ReconcileKind::Waiting
            {
                assert!(
                    !build_dispatched,
                    "replacement Build must wait while the retained source is fenced"
                );
                let mut observed = api.observation().await;
                let RawAgentObservation::PublicV1Report(primary) = observed
                    .agents
                    .get_mut(&primary_key)
                    .expect("retained primary report")
                else {
                    unreachable!()
                };
                primary.role = ReplicaRole::Primary;
                primary.read_status = AccessStatus::Granted;
                primary.write_status = AccessStatus::Granted;
                primary.progress.role = PublicProgressRole::Primary;
                primary.service_location = primary_service_location.clone();
                api.set_observation(observed).await;
                source_waited_before_build = true;
                continue;
            }
            if build_dispatched && !build_seen {
                let build_source = effects[effects_start..]
                    .iter()
                    .find_map(|effect| match effect {
                        EffectRecord::Execute(
                            kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(
                                command,
                            ),
                        ) if matches!(
                            command.intent.program,
                            Some(
                                kuberic_runtime::protocol::public_operations::PublicOperationProgram::Build(_)
                            )
                        ) => Some(&command.target),
                        _ => None,
                    })
                    .expect("Build command source");
                assert!(observed.agents.values().any(|agent| {
                    matches!(
                        agent,
                        RawAgentObservation::PublicV1Report(report)
                            if report.replica == *build_source
                                && report.role == ReplicaRole::Primary
                                && report.write_status == AccessStatus::Granted
                    )
                }));
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
        assert!(
            stable,
            "Public V1 replacement did not converge; source_waited_before_build={source_waited_before_build}; build_seen={build_seen}; observation={:#?}",
            api.observation().await
        );
        assert!(rebind_rejected);
        assert!(source_waited_before_build);
        assert!(build_seen);
        assert!(overlap_injected);
        assert!(admitted_with_authority);
        assert!(predecessor_cleaned_with_authority);
        let bound_target = bound_target.expect("bound replacement target");
        assert!(
            api.effects().await[effects_start..].iter().all(|effect| {
                !matches!(
                    effect,
                    EffectRecord::Execute(
                        kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(
                            command
                        )
                    ) if command.target.replica_id == ReplicaId::new(2)
                        && command.target != bound_target.replica
                )
            }),
            "replacement overlap dispatched topology work to the predecessor incarnation"
        );
        let observed = api.observation().await;
        let replacement = observed
            .agents
            .values()
            .find_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report)
                    if report.storage_incarnation == bound_target =>
                {
                    Some(report.as_ref())
                }
                _ => None,
            })
            .expect("replacement report");
        assert_ne!(replacement.replica, old.1);
        let effects = api.effects().await;
        let replacement_open = effects[effects_start..]
            .iter()
            .position(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if command.target == replacement.replica
                    && matches!(
                        command.intent.program,
                        Some(
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Open {
                                ..
                            }
                        )
                    )
            ))
            .expect("replacement Open effect");
        let replacement_build = effects[effects_start..]
            .iter()
            .position(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if matches!(
                    command.intent.program,
                    Some(
                        kuberic_runtime::protocol::public_operations::PublicOperationProgram::Build(_)
                    )
                )
            ))
            .expect("replacement Build effect");
        let replacement_role = effects[effects_start..]
            .iter()
            .position(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if command.target == replacement.replica
                    && matches!(
                        command.intent.program,
                        Some(
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Role {
                                epoch,
                                role: ReplicaRole::IdleSecondary
                            }
                        ) if epoch == predecessor_epoch
                    )
            ))
            .expect("replacement pre-Build Role effect");
        let replacement_epoch = effects[effects_start..]
            .iter()
            .position(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if command.target == replacement.replica
                    && matches!(
                        command.intent.program,
                        Some(
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Epoch {
                                epoch
                            }
                        ) if epoch == predecessor_epoch
                    )
            ))
            .expect("replacement pre-Build Epoch effect");
        assert!(replacement_open < replacement_build);
        assert!(replacement_open < replacement_role);
        assert!(replacement_epoch < replacement_role);
        assert!(replacement_role < replacement_build);
        let build_command = match &effects[effects_start + replacement_build] {
            EffectRecord::Execute(
                kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command),
            ) => command,
            _ => unreachable!(),
        };
        let retained_revision = effects[..effects_start + replacement_build]
            .iter()
            .rev()
            .find_map(|effect| match effect {
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command),
                ) if command.target == build_command.target
                    && matches!(
                        command.intent.class,
                        kuberic_runtime::protocol::public_operations::PublicOperationClass::Authority
                            | kuberic_runtime::protocol::public_operations::PublicOperationClass::PlannedSwap
                    ) =>
                {
                    Some(command.intent.revision)
                }
                _ => None,
            })
            .expect("retained source authority");
        assert_eq!(build_command.intent.revision, retained_revision);
        let Some(
            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Build(build),
        ) = build_command.intent.program.as_ref()
        else {
            unreachable!()
        };
        assert_eq!(build.epoch, predecessor_epoch);
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
        let primary = topology
            .members
            .iter()
            .find(|member| member.role == ReplicaRole::Primary)
            .expect("replacement primary");
        let primary_epoch = effects[effects_start..]
            .iter()
            .position(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if command.target == primary.identity
                    && matches!(
                        command.intent.program,
                        Some(
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Epoch {
                                epoch
                            }
                        ) if epoch == topology.epoch
                    )
            ))
            .expect("retained primary Epoch effect");
        let primary_role = effects[effects_start..]
            .iter()
            .position(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if command.target == primary.identity
                    && matches!(
                        command.intent.program,
                        Some(
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Role {
                                epoch,
                                role: ReplicaRole::Primary
                            }
                        ) if epoch == topology.epoch
                    )
            ))
            .expect("retained primary Role effect");
        assert!(
            primary_epoch < primary_role,
            "retained primary must advance the epoch before replaying its unchanged role"
        );
        let replacement_epoch = effects[effects_start..]
            .iter()
            .position(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if command.target == replacement.replica
                    && matches!(
                        command.intent.program,
                        Some(
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Epoch {
                                epoch
                            }
                        ) if epoch == topology.epoch
                    )
            ))
            .expect("replacement successor Epoch effect");
        let replacement_role = effects[effects_start..]
            .iter()
            .position(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if command.target == replacement.replica
                    && matches!(
                        command.intent.program,
                        Some(
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Role {
                                epoch,
                                role: ReplicaRole::ActiveSecondary
                            }
                        ) if epoch == topology.epoch
                    )
            ))
            .expect("replacement successor Role effect");
        assert!(
            replacement_epoch < replacement_role,
            "prebuilt replacement must advance the successor epoch before ActiveSecondary role"
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
async fn production_reconciler_finishes_accepted_operation_before_replacement() {
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
        let topology = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .expect("stable topology")
            .configuration()
            .unwrap();
        let target = topology
            .members
            .iter()
            .find(|member| member.identity.replica_id != topology.primary_id)
            .expect("switchover target")
            .identity
            .replica_id;
        observed.set.spec.switchover = Some(PlannedSwitchoverRequestSpec {
            request_id: "replacement-race".into(),
            target_replica_id: u32::try_from(target.value()).unwrap(),
        });
        api.set_observation(observed).await;
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );

        let mut observed = api.observation().await;
        assert!(
            observed
                .set
                .status
                .as_ref()
                .is_some_and(|status| status.public_v1_transition.is_some())
        );
        let victim = topology
            .members
            .iter()
            .find(|member| {
                member.identity.replica_id != topology.primary_id
                    && member.identity.replica_id != target
            })
            .expect("replacement victim")
            .identity
            .clone();
        observed.agents.retain(|_, agent| {
            !matches!(
                agent,
                RawAgentObservation::PublicV1Report(report) if report.replica == victim
            )
        });
        observed
            .pods
            .retain(|pod| pod.uid().as_deref() != Some(victim.instance_id.as_str()));
        api.set_observation(observed).await;

        for _ in 0..8 {
            reconciler.reconcile("default", "sample").await.unwrap();
            if api
                .observation()
                .await
                .set
                .status
                .as_ref()
                .is_some_and(|status| status.public_v1_transition.is_none())
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
                .is_some_and(|status| status.public_v1_transition.is_none())
        );
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        assert!(
            api.observation()
                .await
                .set
                .status
                .as_ref()
                .is_some_and(|status| status.public_v1_replacement.is_some())
        );
    })
    .await
    .expect("accepted-operation replacement ordering exceeded 270 seconds");
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
        assert_eq!(
            observed
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_topology.as_ref())
                .and_then(|topology| topology.completed_switchover_request_id.as_deref()),
            Some("switch-to-2")
        );
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

        let mut observed = observed;
        observed.agents.retain(|_, agent| {
            !matches!(
                agent,
                RawAgentObservation::PublicV1Report(report)
                    if report.replica.replica_id == ReplicaId::new(2)
            )
        });
        api.set_observation(observed).await;
        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        let mut observed = api.observation().await;
        observed.now_unix_seconds = 30;
        api.set_observation(observed).await;
        let mut failed_over = false;
        for _ in 0..160 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                let observed = api.observation().await;
                if observed
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
                    .all(|replica_id| replica_id != ReplicaId::new(2))
                {
                    failed_over = true;
                    break;
                }
            }
        }
        assert!(
            failed_over,
            "completed switchover request blocked later failover: observation={:#?} effects={:#?}",
            api.observation().await,
            api.effects().await
        );
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
        let removed_session = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .and_then(|topology| {
                topology
                    .sessions
                    .iter()
                    .find(|session| session.identity.replica_id == ReplicaId::new(3))
                    .cloned()
            })
            .expect("third topology session");
        observed
            .agents
            .retain(|key, _| key.replica_id != ReplicaId::new(3));
        observed.set.spec.replicas = 2;
        api.set_observation(observed).await;
        let effects_start = api.effects().await.len();
        reconciler.reconcile("default", "sample").await.unwrap();
        let unavailable = api.observation().await;
        assert!(unavailable.pods.iter().any(|pod| {
            pod.uid().as_deref() == Some(removed_session.identity.instance_id.as_str())
        }));
        assert!(
            !api.effects().await[effects_start..]
                .iter()
                .any(|effect| matches!(effect, EffectRecord::DeleteScaffolding { .. })),
            "report unavailability must not authorize accepted-member storage deletion"
        );
        let mut unavailable = unavailable;
        unavailable.pods.retain(|pod| {
            pod.uid().as_deref() != Some(removed_session.identity.instance_id.as_str())
        });
        api.set_observation(unavailable).await;
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
        let removed_pvc_uid = match &removed_session.storage_incarnation.storage {
            kuberic_runtime::protocol::types::ReplicaStorageIdentity::Persistent {
                pvc_uid,
                ..
            } => pvc_uid,
            kuberic_runtime::protocol::types::ReplicaStorageIdentity::Volatile { .. } => {
                panic!("expected persistent test storage")
            }
        };
        assert!(
            !observed
                .pvcs
                .iter()
                .any(|pvc| pvc.uid().as_deref() == Some(removed_pvc_uid.as_str()))
        );
        assert!(!observed.services.iter().any(|service| {
            service.spec.as_ref().is_some_and(|spec| {
                spec.selector.as_ref().is_some_and(|selector| {
                    selector
                        .get(kuberic_controller::crd::INSTANCE_LABEL)
                        .is_some_and(|instance| {
                            instance == removed_session.identity.instance_id.as_str()
                        })
                })
            })
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

        let mut observed = observed;
        observed.set.spec.replicas = 3;
        api.set_observation(observed).await;
        let mut rescaled = false;
        for _ in 0..200 {
            if reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind
                == ReconcileKind::Stable
            {
                rescaled = true;
                break;
            }
        }
        assert!(rescaled, "fresh scale-up after cleanup did not converge");
        let observed = api.observation().await;
        let replacement_session = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .and_then(|topology| {
                topology
                    .sessions
                    .iter()
                    .find(|session| session.identity.replica_id == ReplicaId::new(3))
            })
            .expect("rescaled third topology session");
        assert_ne!(
            replacement_session.storage_incarnation,
            removed_session.storage_incarnation
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
        let primary_key = observed
            .agents
            .keys()
            .find(|key| key.replica_id == ReplicaId::new(3))
            .cloned()
            .expect("third replica report");
        let primary_agent = observed
            .agents
            .remove(&primary_key)
            .expect("third replica observation");
        api.set_observation(observed).await;
        let start = api.effects().await.len();
        reconciler.reconcile("default", "sample").await.unwrap();
        let mut unavailable_primary = api.observation().await;
        assert!(unavailable_primary.pods.iter().any(|pod| {
            pod.uid().as_deref() == Some(primary_key.instance_id.as_str())
        }));
        assert!(
            !api.effects().await[start..]
                .iter()
                .any(|effect| matches!(effect, EffectRecord::DeleteScaffolding { .. })),
            "unavailable accepted primary must not be deleted before safe transfer"
        );
        unavailable_primary
            .agents
            .insert(primary_key, primary_agent);
        api.set_observation(unavailable_primary).await;
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
        let handoff = new_effects
            .iter()
            .position(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if command.intent.operation_id.as_str().contains("-handoff-") && matches!(
                    command.intent.program,
                    Some(
                        kuberic_runtime::protocol::public_operations::PublicOperationProgram::Swap { .. }
                    )
                )
            ))
            .expect("durable source handoff effect");
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
        assert!(catch_up < handoff && handoff < route && route < remove);
    })
    .await
    .expect("Public V1 primary removal ordering exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_replays_topology_after_process_restart() {
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
        restarted.process_session_id =
            kuberic_runtime::protocol::types::ProcessSessionId::new("restarted-primary-session");
        restarted.replicator_creation_id =
            kuberic_runtime::protocol::types::ReplicatorCreationId::new(
                "pending-restarted-primary-session",
            );
        restarted.report_sequence = 1;
        restarted.role = ReplicaRole::None;
        restarted.read_status = AccessStatus::NotPrimary;
        restarted.write_status = AccessStatus::NotPrimary;
        restarted.progress.role = PublicProgressRole::Recovery;
        restarted.service_location = None;
        restarted.completed_operations.clear();
        let predecessor = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .and_then(|topology| {
                topology
                    .sessions
                    .iter()
                    .find(|session| session.identity == restarted.replica)
                    .cloned()
            })
            .expect("accepted predecessor session");
        let previous_epoch = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .expect("accepted topology")
            .epoch;
        observed.agents.insert(
            key.clone(),
            RawAgentObservation::PublicV1Report(Box::new(restarted.clone())),
        );
        api.set_observation(observed).await;
        let effects_start = api.effects().await.len();

        let mut opened = None;
        for _ in 0..20 {
            reconciler.reconcile("default", "sample").await.unwrap();
            let observation = api.observation().await;
            assert!(
                observation
                    .set
                    .status
                    .as_ref()
                    .and_then(|status| status.public_v1_session_renewal.as_ref())
                    .is_none()
            );
            let open_effects = api.effects().await;
            assert!(open_effects[effects_start..].iter().all(|effect| {
                !matches!(
                    effect,
                    EffectRecord::Execute(
                        kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                    ) if command.target == restarted.replica
                        && matches!(
                            command.intent.program,
                            Some(
                                kuberic_runtime::protocol::public_operations::
                                    PublicOperationProgram::Epoch { .. }
                                    | kuberic_runtime::protocol::public_operations::
                                        PublicOperationProgram::Role { .. }
                                    | kuberic_runtime::protocol::public_operations::
                                        PublicOperationProgram::Configuration(_)
                            )
                        )
                )
            }));
            if open_effects[effects_start..].iter().any(|effect| matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                ) if command.target == restarted.replica
                    && matches!(
                        command.intent.program,
                        Some(
                            kuberic_runtime::protocol::public_operations::
                                PublicOperationProgram::Open { existing: true, .. }
                        )
                    )
            )) {
                opened = Some(observation);
                break;
            }
        }
        let mut opened = opened.expect("contained restart Open was not dispatched");
        let RawAgentObservation::PublicV1Report(opened_report) =
            opened.agents.get_mut(&key).expect("opened restart report")
        else {
            panic!("expected Public V1 restart report");
        };
        opened_report.replicator_creation_id =
            kuberic_runtime::protocol::types::ReplicatorCreationId::new(
                "restarted-primary-creation",
            );
        restarted.replicator_creation_id = opened_report.replicator_creation_id.clone();
        api.set_observation(opened).await;

        let renewal_effects_start = api.effects().await.len();
        let mut admitted_renewal = None;
        for _ in 0..20 {
            reconciler.reconcile("default", "sample").await.unwrap();
            let observation = api.observation().await;
            let renewal_effects = api.effects().await;
            assert!(renewal_effects[renewal_effects_start..].iter().all(|effect| {
                !matches!(
                    effect,
                    EffectRecord::Execute(
                        kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(command)
                    ) if command.target == restarted.replica
                        && matches!(
                            command.intent.program,
                            Some(
                                kuberic_runtime::protocol::public_operations::
                                    PublicOperationProgram::Epoch { .. }
                                    | kuberic_runtime::protocol::public_operations::
                                        PublicOperationProgram::Role { .. }
                                    | kuberic_runtime::protocol::public_operations::
                                        PublicOperationProgram::Configuration(_)
                            )
                        )
                )
            }));
            if let Some(renewal) = observation
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_session_renewal.as_ref())
            {
                admitted_renewal = Some(renewal.renewal().unwrap());
                break;
            }
        }
        let renewal = admitted_renewal.expect("contained restart must persist renewal");
        assert_eq!(renewal.required.predecessor, predecessor);
        assert_eq!(
            renewal
                .required
                .replacement
                .as_ref()
                .expect("replacement endpoint")
                .process_session_id,
            restarted.process_session_id
        );
        assert_eq!(renewal.required.previous_epoch, previous_epoch);
        assert!(renewal.required.renewal_epoch > previous_epoch);
        let renewal_epoch = renewal.required.renewal_epoch;

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
        assert!(stable, "topology replay did not converge");
        let observed = api.observation().await;
        let status = observed.set.status.as_ref().expect("status");
        assert!(status.public_v1_session_renewal.is_none());
        assert!(
            status
                .public_v1_topology
                .as_ref()
                .expect("topology")
                .sessions
                .iter()
                .any(|session| {
                    session.identity == restarted.replica
                        && session.process_session_id == restarted.process_session_id
                        && session.replicator_creation_id == restarted.replicator_creation_id
                })
        );
        assert_eq!(
            status.public_v1_topology.as_ref().expect("topology").epoch,
            renewal_epoch
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
        let effects = api.effects().await;
        assert!(effects[effects_start..].iter().any(|effect| matches!(
            effect,
            EffectRecord::Execute(
                kuberic_runtime::protocol::command::ProtocolCommand::PublicV1History(command)
            ) if matches!(
                command.as_ref(),
                kuberic_runtime::protocol::command::PublicV1HistoryCommand::SessionRenewal {
                    ..
                }
            )
        )));
    })
    .await
    .expect("Public V1 topology replay exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_renews_already_admitted_restarted_session() {
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
        let (key, predecessor, replacement) = observed
            .agents
            .iter_mut()
            .find_map(|(key, agent)| match agent {
                RawAgentObservation::PublicV1Report(report)
                    if report.role == ReplicaRole::ActiveSecondary =>
                {
                    let predecessor = report.as_ref().clone();
                    report.process_session_id =
                        kuberic_runtime::protocol::types::ProcessSessionId::new(
                            "already-admitted-restarted-session",
                        );
                    report.replicator_creation_id =
                        kuberic_runtime::protocol::types::ReplicatorCreationId::new(
                            "already-admitted-restarted-creation",
                        );
                    report.report_sequence = 1;
                    Some((key.clone(), predecessor, report.as_ref().clone()))
                }
                _ => None,
            })
            .expect("admitted secondary");
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
        let renewal = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_session_renewal.as_ref())
            .expect("session renewal requirement")
            .renewal()
            .unwrap();
        assert_eq!(
            renewal.stage,
            kuberic_runtime::protocol::public_operations::PublicSessionRenewalStage::Required
        );
        assert_eq!(
            renewal.required.predecessor.process_session_id,
            predecessor.process_session_id
        );
        let endpoint = renewal.required.replacement.as_ref().unwrap();
        assert_eq!(endpoint.process_session_id, replacement.process_session_id);
        assert_eq!(
            endpoint.replicator_creation_id,
            replacement.replicator_creation_id
        );
        assert_eq!(endpoint.role, ReplicaRole::None);

        let effects_start = api.effects().await.len();
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
        assert!(stable, "admitted session renewal did not converge");
        let observed = api.observation().await;
        let status = observed.set.status.as_ref().expect("status");
        assert!(status.public_v1_session_renewal.is_none());
        assert!(
            status
                .public_v1_topology
                .as_ref()
                .expect("topology")
                .sessions
                .iter()
                .any(|session| {
                    session.identity == replacement.replica
                        && session.process_session_id == replacement.process_session_id
                        && session.replicator_creation_id == replacement.replicator_creation_id
                })
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
        assert!(api.effects().await[effects_start..].iter().any(|effect| {
            matches!(
                effect,
                EffectRecord::Execute(
                    kuberic_runtime::protocol::command::ProtocolCommand::PublicV1History(command)
                ) if matches!(
                    command.as_ref(),
                    kuberic_runtime::protocol::command::PublicV1HistoryCommand::SessionRenewal {
                        ..
                    }
                )
            )
        }));
        assert!(observed.agents.contains_key(&key));
    })
    .await
    .expect("admitted session renewal exceeded 270 seconds");
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

#[tokio::test]
async fn production_reconciler_completes_persisted_evidence_after_session_change() {
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

        reconciler.reconcile("default", "sample").await.unwrap();
        let mut observed = api.observation().await;
        let status = observed.set.status.as_ref().expect("status");
        let transition = status
            .public_v1_transition
            .as_ref()
            .expect("persisted transition")
            .transition()
            .unwrap();
        assert!(
            transition
                .evidence
                .completed_operations
                .iter()
                .any(|completion| completion.operation_id == transition.intent.operation_id)
        );
        assert!(status.public_v1_operation.is_some());

        let report = observed
            .agents
            .values_mut()
            .find_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report) => Some(report.as_mut()),
                _ => None,
            })
            .expect("established report");
        report.process_session_id =
            kuberic_runtime::protocol::types::ProcessSessionId::new("session-after-persist");
        report.replicator_creation_id =
            kuberic_runtime::protocol::types::ReplicatorCreationId::new("creation-after-persist");
        api.set_observation(observed).await;

        let executes_before = api
            .effects()
            .await
            .iter()
            .filter(|effect| matches!(effect, EffectRecord::Execute(_)))
            .count();
        reconciler.reconcile("default", "sample").await.unwrap();
        let executes_after = api
            .effects()
            .await
            .iter()
            .filter(|effect| matches!(effect, EffectRecord::Execute(_)))
            .count();
        assert_eq!(executes_after, executes_before);
        let observed = api.observation().await;
        let status = observed.set.status.as_ref().expect("status");
        assert!(status.public_v1_transition.is_none());
        assert!(status.public_v1_operation.is_none());
    })
    .await
    .expect("persisted completion replay exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_accepts_terminal_predecessor_evidence_after_status_conflict() {
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
        reconciler.reconcile("default", "sample").await.unwrap();
        let mut observed = api.observation().await;
        let operation_id = {
            let status = observed.set.status.as_mut().expect("status");
            let mut transition = status
                .public_v1_transition
                .as_ref()
                .expect("pending transition")
                .transition()
                .unwrap();
            let operation_id = transition.intent.operation_id.clone();
            assert!(
                transition
                    .evidence
                    .completed_operations
                    .iter()
                    .any(|completion| completion.operation_id == operation_id)
            );
            transition
                .evidence
                .completed_operations
                .retain(|completion| completion.operation_id != operation_id);
            status.public_v1_transition = Some(
                kuberic_controller::crd::PublicV1TransitionState::from_transition(&transition)
                    .unwrap(),
            );
            let mut authority = status
                .public_v1_authority
                .as_ref()
                .expect("authority")
                .authority()
                .unwrap();
            authority
                .completed_operations
                .retain(|completion| completion.operation_id != operation_id);
            status.public_v1_authority = Some(
                kuberic_controller::crd::PublicV1AuthorityStatus::from_authority(&authority)
                    .unwrap(),
            );
            operation_id
        };
        let report = observed
            .agents
            .values_mut()
            .find_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report) => Some(report.as_mut()),
                _ => None,
            })
            .expect("terminal predecessor report");
        assert!(
            report
                .completed_operations
                .iter()
                .any(|completion| completion.operation_id == operation_id)
        );
        report.process_session_id =
            kuberic_runtime::protocol::types::ProcessSessionId::new("successor-session");
        report.replicator_creation_id =
            kuberic_runtime::protocol::types::ReplicatorCreationId::new("successor-creation");
        api.set_observation(observed).await;

        let executes_before = api
            .effects()
            .await
            .iter()
            .filter(|effect| matches!(effect, EffectRecord::Execute(_)))
            .count();
        reconciler.reconcile("default", "sample").await.unwrap();
        reconciler.reconcile("default", "sample").await.unwrap();
        let executes_after = api
            .effects()
            .await
            .iter()
            .filter(|effect| matches!(effect, EffectRecord::Execute(_)))
            .count();
        assert_eq!(executes_after, executes_before);
        let observed = api.observation().await;
        let status = observed.set.status.as_ref().expect("status");
        assert!(status.public_v1_transition.is_none());
        assert!(status.public_v1_operation.is_none());
    })
    .await
    .expect("terminal predecessor evidence reconciliation exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_supersedes_incomplete_predecessor_session() {
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
        let mut observed = api.observation().await;
        let report = observed
            .agents
            .values_mut()
            .find_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report) => Some(report.as_mut()),
                _ => None,
            })
            .expect("established report");
        report.process_session_id =
            kuberic_runtime::protocol::types::ProcessSessionId::new("replacement-session");
        report.replicator_creation_id =
            kuberic_runtime::protocol::types::ReplicatorCreationId::new("replacement-creation");
        report.completed_operations.clear();
        api.set_observation(observed).await;
        let executes_before = api
            .effects()
            .await
            .iter()
            .filter(|effect| matches!(effect, EffectRecord::Execute(_)))
            .count();
        reconciler.reconcile("default", "sample").await.unwrap();
        let executes_after = api
            .effects()
            .await
            .iter()
            .filter(|effect| matches!(effect, EffectRecord::Execute(_)))
            .count();
        assert_eq!(executes_after, executes_before);
        let observed = api.observation().await;
        let status = observed.set.status.as_ref().expect("status");
        assert!(status.public_v1_transition.is_none());
        assert!(status.public_v1_operation.is_none());
    })
    .await
    .expect("incomplete predecessor supersession exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_supersedes_operation_after_source_pod_loss() {
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
        let mut observed = api.observation().await;
        let source = observed
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_transition.as_ref())
            .expect("accepted transition")
            .transition()
            .unwrap()
            .intent
            .source
            .storage_incarnation
            .replica;
        observed
            .pods
            .retain(|pod| pod.uid().as_deref() != Some(source.instance_id.as_str()));
        observed.agents.retain(|key, _| {
            key.replica_id != source.replica_id || key.instance_id != source.instance_id
        });
        api.set_observation(observed).await;
        let executes_before = api
            .effects()
            .await
            .iter()
            .filter(|effect| matches!(effect, EffectRecord::Execute(_)))
            .count();

        reconciler.reconcile("default", "sample").await.unwrap();

        let executes_after = api
            .effects()
            .await
            .iter()
            .filter(|effect| matches!(effect, EffectRecord::Execute(_)))
            .count();
        assert_eq!(executes_after, executes_before);
        let observed = api.observation().await;
        let status = observed.set.status.as_ref().expect("status");
        assert!(status.public_v1_transition.is_none());
        assert!(status.public_v1_operation.is_none());
    })
    .await
    .expect("lost-source supersession exceeded 270 seconds");
}

#[tokio::test]
async fn production_reconciler_refreshes_primary_configuration_for_new_peer_session() {
    tokio::time::timeout(Duration::from_secs(270), async {
        let api = Arc::new(InMemoryClusterApi::new(empty_observation(3)));
        let reconciler = Reconciler::new(
            api.clone(),
            kuberic_controller::production_evaluation_config(0, 1, 1),
        );
        for _ in 0..160 {
            reconciler.reconcile("default", "sample").await.unwrap();
            let observed = api.observation().await;
            if observed.set.status.as_ref().is_some_and(|status| {
                status.public_v1_topology.is_none()
                    && status.public_v1_transition.is_none()
                    && status
                        .public_v1_authority
                        .as_ref()
                        .is_some_and(|authority| authority.completed_operation_count >= 12)
            }) {
                break;
            }
        }

        let mut observed = api.observation().await;
        assert!(
            observed
                .set
                .status
                .as_ref()
                .is_some_and(|status| status.public_v1_topology.is_none()),
            "test must interrupt initial topology before it is persisted"
        );
        let primary = observed
            .agents
            .values()
            .find_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report)
                    if report.role == ReplicaRole::Primary =>
                {
                    Some(report.replica.clone())
                }
                _ => None,
            })
            .expect("initial primary");
        let peer = observed
            .agents
            .values_mut()
            .find_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report)
                    if report.replica != primary && report.role == ReplicaRole::ActiveSecondary =>
                {
                    Some(report.as_mut())
                }
                _ => None,
            })
            .expect("initial secondary");
        peer.process_session_id =
            kuberic_runtime::protocol::types::ProcessSessionId::new("restarted-peer-session");
        peer.replicator_creation_id =
            kuberic_runtime::protocol::types::ReplicatorCreationId::new("restarted-peer-creation");
        peer.report_sequence = 1;
        peer.role = ReplicaRole::None;
        peer.read_status = AccessStatus::NotPrimary;
        peer.write_status = AccessStatus::NotPrimary;
        peer.progress.role = PublicProgressRole::Recovery;
        peer.service_location = None;
        peer.completed_operations.clear();
        api.set_observation(observed).await;

        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        let command = api
            .observation()
            .await
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_operation.as_ref())
            .expect("refreshed primary configuration")
            .command()
            .unwrap();
        assert_eq!(command.target, primary);
        assert!(matches!(
            command.intent.program.as_ref(),
            Some(
                kuberic_runtime::protocol::public_operations::PublicOperationProgram::Configuration(
                    _
                )
            )
        ));

        let mut observed = api.observation().await;
        let report = observed
            .agents
            .values_mut()
            .find_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report)
                    if report.replica == command.target =>
                {
                    Some(report.as_mut())
                }
                _ => None,
            })
            .expect("primary report");
        let Some(
            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Configuration(
                configuration,
            ),
        ) = command.intent.program.as_ref()
        else {
            unreachable!()
        };
        report.completed_operations.push(
            kuberic_runtime::protocol::public_operations::PublicV1OperationCompletion {
                operation_id: command.intent.operation_id.clone(),
                revision: command.intent.revision,
                call_kind:
                    kuberic_runtime::protocol::public_operations::PublicV1CallKind::Configuration,
                process_session_id: command.intent.process_session_id.clone(),
                input_digest: command.intent.input_digest.clone(),
                replay_disposition: command.intent.replay_disposition,
                history: report.history.clone(),
                storage_incarnation: report.storage_incarnation.clone(),
                previous_configuration_id: configuration
                    .previous
                    .as_ref()
                    .map(|previous| previous.configuration_id.clone()),
                current_configuration_id: Some(
                    configuration.current.configuration_id.clone(),
                ),
                catch_up_mode: None,
                catch_up_specific_quorum: None,
                result:
                    kuberic_runtime::protocol::public_operations::PublicV1OperationResult::Succeeded,
            },
        );
        let status = observed.set.status.as_mut().expect("status");
        status.public_v1_transition = None;
        status.public_v1_operation = None;
        api.set_observation(observed).await;

        assert_eq!(
            reconciler
                .reconcile("default", "sample")
                .await
                .unwrap()
                .kind,
            ReconcileKind::Applied
        );
        let replay = api
            .observation()
            .await
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_operation.as_ref())
            .expect("reconstructed primary configuration")
            .command()
            .unwrap();
        assert_eq!(replay.intent.operation_id, command.intent.operation_id);
        assert_eq!(replay.intent.revision, command.intent.revision);
        assert_eq!(replay.intent.input_digest, command.intent.input_digest);
    })
    .await
    .expect("peer-session configuration refresh exceeded 270 seconds");
}
