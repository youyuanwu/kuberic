use std::collections::{BTreeMap, BTreeSet};

use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::ResourceExt;
use kuberic_protocol::observation::{
    AgentObservation, DesiredState, KubernetesReplicaObservation, ObservationFailure,
    ObservationSnapshot, ReplicaObservation, ReplicaObservationKey, ReportWatermark,
    RoutingObservation,
};
use kuberic_protocol::types::{
    AcceptedStatus, PodUid, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ResourceUid,
};

use crate::crd::{INSTANCE_LABEL, REPLICA_ID_LABEL, SET_UID_LABEL, STORAGE_STATE_ANNOTATION};
use crate::observation::{RawAgentObservation, RawObservation};
use crate::{ControllerError, Result};

pub fn normalize(
    raw: RawObservation,
    previous_report_watermarks: BTreeMap<ReplicaObservationKey, ReportWatermark>,
) -> Result<ObservationSnapshot> {
    let resource_uid = raw
        .set
        .uid()
        .filter(|uid| !uid.is_empty())
        .map(ResourceUid::new)
        .ok_or_else(|| ControllerError::Observation("KubericSet has no UID".to_string()))?;
    let resource_version = raw
        .set
        .resource_version()
        .filter(|version| !version.is_empty())
        .ok_or_else(|| {
            ControllerError::Observation("KubericSet has no resourceVersion".to_string())
        })?;
    let generation = raw.set.metadata.generation.unwrap_or_default();
    if generation < 0 {
        return Err(ControllerError::Observation(
            "KubericSet generation is negative".to_string(),
        ));
    }

    let status = raw
        .set
        .status
        .as_ref()
        .map(|status| status.authority.clone())
        .unwrap_or_default();
    let mut failures = raw
        .failures
        .iter()
        .cloned()
        .map(|failure| ObservationFailure {
            source: failure.source,
            message: failure.message,
        })
        .collect::<Vec<_>>();
    let mut replica_ids = authority_replica_ids(&status);
    replica_ids.extend((1..=raw.set.spec.replicas).map(|value| ReplicaId::new(i64::from(value))));
    collect_labeled_replica_ids(&raw.pods, &mut replica_ids, &mut failures, "Pod");
    collect_labeled_replica_ids(&raw.pvcs, &mut replica_ids, &mut failures, "PVC");

    let mut replicas = BTreeMap::new();
    let mut durable_storage_evidence = false;
    for replica_id in replica_ids {
        let pods = matching_objects(&raw.pods, &resource_uid, replica_id);
        let pvcs = matching_objects(&raw.pvcs, &resource_uid, replica_id);
        if pods.len() > 1 {
            failures.push(ObservationFailure {
                source: format!("replica/{replica_id}/pods"),
                message: "multiple owned Pods claim one logical replica".to_string(),
            });
        }
        if pvcs.len() > 1 {
            failures.push(ObservationFailure {
                source: format!("replica/{replica_id}/pvcs"),
                message: "multiple owned PVCs claim one logical replica".to_string(),
            });
        }
        let pod = (pods.len() == 1).then(|| pods[0]);
        let pvc = (pvcs.len() == 1).then(|| pvcs[0]);
        durable_storage_evidence |= pvc.is_some_and(established_storage);

        let pod_uid = pod.and_then(|pod| pod.uid()).map(PodUid::new);
        let pvc_uid = pvc.and_then(|pvc| pvc.uid()).map(PvcUid::new);
        let kubernetes = (pod.is_some() || pvc.is_some()).then(|| KubernetesReplicaObservation {
            replica_id,
            pod_name: pod.map(ResourceExt::name_any).unwrap_or_default(),
            pod_uid: pod_uid.clone(),
            pvc_name: pvc.map(ResourceExt::name_any).unwrap_or_default(),
            pvc_uid: pvc_uid.clone(),
            pod_ready: pod.is_some_and(pod_ready),
        });
        let agent = normalize_agent(
            raw.agents
                .get(&replica_id)
                .cloned()
                .unwrap_or(RawAgentObservation::Absent),
            &resource_uid,
            replica_id,
            pod_uid.as_ref(),
            pvc_uid.as_ref(),
            &previous_report_watermarks,
        );
        let instance_id = observation_instance_id(&agent, pod_uid.as_ref(), replica_id);
        replicas.insert(
            ReplicaObservationKey::new(replica_id, instance_id),
            ReplicaObservation { kubernetes, agent },
        );
    }

    let routing = normalize_routing(&raw, &replicas, &resource_uid, &mut failures);
    Ok(ObservationSnapshot {
        resource_uid,
        resource_version,
        desired: DesiredState {
            generation: generation as u64,
            replicas: raw.set.spec.replicas,
            image: raw.set.spec.image,
            failover_delay_seconds: raw.set.spec.failover_delay_seconds,
        },
        status,
        replicas,
        previous_report_watermarks,
        durable_storage_evidence,
        routing,
        observation_failures: failures,
        now_unix_seconds: raw.now_unix_seconds,
    })
}

pub fn report_watermarks(
    snapshot: &ObservationSnapshot,
) -> BTreeMap<ReplicaObservationKey, ReportWatermark> {
    snapshot
        .replicas
        .iter()
        .filter_map(|(key, observation)| match &observation.agent {
            AgentObservation::Uninitialized(report) => Some((
                key.clone(),
                ReportWatermark {
                    process_session_id: report.process_session_id.clone(),
                    report_sequence: report.report_sequence,
                },
            )),
            AgentObservation::Report(report) => Some((
                key.clone(),
                ReportWatermark {
                    process_session_id: report.process_session_id.clone(),
                    report_sequence: report.report_sequence,
                },
            )),
            AgentObservation::Absent
            | AgentObservation::Unreachable { .. }
            | AgentObservation::Invalid { .. } => None,
        })
        .collect()
}

fn authority_replica_ids(status: &AcceptedStatus) -> BTreeSet<ReplicaId> {
    status
        .topology
        .iter()
        .flat_map(|topology| &topology.configuration.members)
        .chain(
            status
                .transition
                .iter()
                .flat_map(|transition| &transition.current_configuration.members),
        )
        .map(|member| member.identity.replica_id)
        .chain(status.provisioning.iter().map(|intent| intent.replica_id))
        .collect()
}

fn collect_labeled_replica_ids<K>(
    objects: &[K],
    replica_ids: &mut BTreeSet<ReplicaId>,
    failures: &mut Vec<ObservationFailure>,
    kind: &str,
) where
    K: ResourceExt,
{
    for object in objects {
        let Some(value) = object.labels().get(REPLICA_ID_LABEL) else {
            continue;
        };
        match value.parse::<i64>() {
            Ok(value) if value > 0 => {
                replica_ids.insert(ReplicaId::new(value));
            }
            _ => failures.push(ObservationFailure {
                source: format!("{kind}/{}", object.name_any()),
                message: "replica-id label is not a positive integer".to_string(),
            }),
        }
    }
}

fn matching_objects<'a, K>(
    objects: &'a [K],
    resource_uid: &ResourceUid,
    replica_id: ReplicaId,
) -> Vec<&'a K>
where
    K: ResourceExt,
{
    objects
        .iter()
        .filter(|object| {
            object.labels().get(SET_UID_LABEL).map(String::as_str) == Some(resource_uid.as_str())
                && object
                    .labels()
                    .get(REPLICA_ID_LABEL)
                    .is_some_and(|value| value == &replica_id.to_string())
        })
        .collect()
}

fn normalize_agent(
    raw: RawAgentObservation,
    resource_uid: &ResourceUid,
    replica_id: ReplicaId,
    pod_uid: Option<&PodUid>,
    pvc_uid: Option<&PvcUid>,
    previous: &BTreeMap<ReplicaObservationKey, ReportWatermark>,
) -> AgentObservation {
    let observation = match raw {
        RawAgentObservation::Absent => AgentObservation::Absent,
        RawAgentObservation::Unavailable { message } => AgentObservation::Unreachable { message },
        RawAgentObservation::Invalid { message } => AgentObservation::Invalid { message },
        RawAgentObservation::Report(report) => {
            match kuberic_wire::normalize_agent_status_report(*report) {
                Ok(observation) => observation,
                Err(error) => {
                    return AgentObservation::Invalid {
                        message: error.to_string(),
                    };
                }
            }
        }
    };
    let mismatch = match &observation {
        AgentObservation::Uninitialized(report) => (report.resource_uid != *resource_uid)
            .then_some("uninitialized report resource UID differs from the KubericSet")
            .or_else(|| {
                (report.replica_id != replica_id)
                    .then_some("uninitialized report replica ID differs from its Pod")
            })
            .or_else(|| {
                (pod_uid != Some(&report.pod_uid))
                    .then_some("uninitialized report Pod UID differs from Kubernetes")
            })
            .or_else(|| {
                (pvc_uid != Some(&report.pvc_uid))
                    .then_some("uninitialized report PVC UID differs from Kubernetes")
            }),
        AgentObservation::Report(report) => (report.resource_uid != *resource_uid)
            .then_some("agent report resource UID differs from the KubericSet")
            .or_else(|| {
                (report.identity.replica_id != replica_id)
                    .then_some("agent report replica ID differs from its Pod")
            })
            .or_else(|| {
                pod_uid
                    .is_some_and(|uid| uid.as_str() != report.identity.instance_id.as_str())
                    .then_some("agent identity differs from the current Pod UID")
            }),
        AgentObservation::Absent
        | AgentObservation::Unreachable { .. }
        | AgentObservation::Invalid { .. } => None,
    };
    if let Some(message) = mismatch {
        return AgentObservation::Invalid {
            message: message.to_string(),
        };
    }

    let watermark = match &observation {
        AgentObservation::Uninitialized(report) => Some((
            ReplicaObservationKey::new(
                report.replica_id,
                ReplicaInstanceId::new(report.pod_uid.as_str()),
            ),
            &report.process_session_id,
            report.report_sequence,
        )),
        AgentObservation::Report(report) => Some((
            ReplicaObservationKey::new(
                report.identity.replica_id,
                report.identity.instance_id.clone(),
            ),
            &report.process_session_id,
            report.report_sequence,
        )),
        AgentObservation::Absent
        | AgentObservation::Unreachable { .. }
        | AgentObservation::Invalid { .. } => None,
    };
    if let Some((key, session, sequence)) = watermark
        && previous.get(&key).is_some_and(|previous| {
            previous.process_session_id == *session && sequence < previous.report_sequence
        })
    {
        return AgentObservation::Invalid {
            message: "agent report sequence regressed within one process session".to_string(),
        };
    }
    observation
}

fn observation_instance_id(
    agent: &AgentObservation,
    pod_uid: Option<&PodUid>,
    replica_id: ReplicaId,
) -> ReplicaInstanceId {
    match agent {
        AgentObservation::Uninitialized(report) => ReplicaInstanceId::new(report.pod_uid.as_str()),
        AgentObservation::Report(report) => report.identity.instance_id.clone(),
        AgentObservation::Absent
        | AgentObservation::Unreachable { .. }
        | AgentObservation::Invalid { .. } => pod_uid
            .map(|uid| ReplicaInstanceId::new(uid.as_str()))
            .unwrap_or_else(|| ReplicaInstanceId::new(format!("missing-{replica_id}"))),
    }
}

fn established_storage(pvc: &PersistentVolumeClaim) -> bool {
    pvc.annotations()
        .get(STORAGE_STATE_ANNOTATION)
        .is_some_and(|value| value == "initialized")
}

fn pod_ready(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .is_some_and(|conditions| {
            conditions
                .iter()
                .any(|condition| condition.type_ == "Ready" && condition.status == "True")
        })
}

fn normalize_routing(
    raw: &RawObservation,
    replicas: &BTreeMap<ReplicaObservationKey, ReplicaObservation>,
    resource_uid: &ResourceUid,
    failures: &mut Vec<ObservationFailure>,
) -> RoutingObservation {
    let write_services = raw
        .services
        .iter()
        .filter(|service| {
            service.labels().get(SET_UID_LABEL).map(String::as_str) == Some(resource_uid.as_str())
                && service.name_any().ends_with("-write")
        })
        .collect::<Vec<_>>();
    if write_services.len() > 1 {
        failures.push(ObservationFailure {
            source: "routing".to_string(),
            message: "multiple write Services claim the KubericSet".to_string(),
        });
        return RoutingObservation::default();
    }
    let Some(instance) = write_services
        .first()
        .and_then(|service| service.spec.as_ref())
        .and_then(|spec| spec.selector.as_ref())
        .and_then(|selector| selector.get(INSTANCE_LABEL))
        .filter(|instance| instance.as_str() != "disabled")
    else {
        return RoutingObservation::default();
    };
    let matches = replicas
        .values()
        .filter_map(|observation| match &observation.agent {
            AgentObservation::Report(report)
                if report.identity.instance_id.as_str() == instance.as_str() =>
            {
                Some(report.identity.clone())
            }
            _ => None,
        })
        .collect::<Vec<ReplicaIdentity>>();
    if matches.len() == 1 {
        RoutingObservation {
            write_target: matches.into_iter().next(),
        }
    } else {
        failures.push(ObservationFailure {
            source: "routing".to_string(),
            message: "write Service selector does not identify one observed replica".to_string(),
        });
        RoutingObservation::default()
    }
}
