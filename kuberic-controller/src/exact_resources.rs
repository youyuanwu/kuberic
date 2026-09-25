use k8s_openapi::api::core::v1::{PersistentVolumeClaim, Pod};
use kube::ResourceExt;
use kuberic_protocol::observation::{
    ExactResourceObservation, SecondaryScaleDownResourceObservation,
};
use kuberic_protocol::types::{
    CleanupResourceIdentity, PodUid, PvcUid, ReplicaCleanupIdentity, ReplicaIdentity, ReplicaRole,
    ResourceUid, derive_agent_generation, derive_initialization_id, derive_replica_endpoint_name,
};

use crate::crd::{INSTANCE_LABEL, SET_UID_LABEL};
use crate::observation::{
    ExactLookup, RawObservation, RawObservationFailure, RawScaleDownResources,
};

pub(crate) fn requests(
    raw: &RawObservation,
) -> Vec<(ReplicaIdentity, ReplicaCleanupIdentity, bool)> {
    let Some(status) = raw.set.status.as_ref().map(|s| &s.authority) else {
        return Vec::new();
    };
    if let Some(intent) = status
        .secondary_scale_down_cleanup
        .as_ref()
        .map(|c| &c.evidence.preparation.intent)
        .or_else(|| status.transition.as_ref()?.secondary_scale_down.as_ref())
    {
        return vec![(intent.target.clone(), intent.cleanup.clone(), true)];
    }
    let Some(topology) = status.topology.as_ref() else {
        return Vec::new();
    };
    if raw.set.spec.replicas == 0
        || raw.set.spec.replicas as usize >= topology.configuration.members.len()
    {
        return Vec::new();
    }
    let Some(target) = topology
        .configuration
        .members
        .iter()
        .filter(|m| m.role == ReplicaRole::ActiveSecondary)
        .max_by_key(|m| m.identity.replica_id)
        .map(|m| &m.identity)
    else {
        return Vec::new();
    };
    let resource_uid = ResourceUid::new(raw.set.uid().unwrap_or_default());
    let pod = raw
        .pods
        .iter()
        .find(|p| p.uid().as_deref() == Some(target.instance_id.as_str()));
    let pvc = raw.pvcs.iter().find(|pvc| {
        let Some(uid) = pvc.uid() else { return false };
        derive_agent_generation(&derive_initialization_id(
            &resource_uid,
            target.replica_id,
            &PodUid::new(target.instance_id.as_str()),
            &PvcUid::new(uid),
        )) == target.agent_generation
    });
    let Some(pvc) = pvc else { return Vec::new() };
    let pvc_name = pvc.name_any();
    let pod_name = if let Some(pod) = pod {
        // The durable generation binds this PVC UID; the actual mount must agree too.
        if mounted_pvc(pod) != Some(pvc_name.as_str()) {
            return Vec::new();
        }
        pod.name_any()
    } else {
        // Both scaffold constructors use <pod>-data. The generation proves storage
        // provenance; a subsequent exact Pod GET, not list omission, proves absence.
        let Some(name) = pvc_name.strip_suffix("-data") else {
            return Vec::new();
        };
        name.to_string()
    };
    let endpoint_name = derive_replica_endpoint_name(&resource_uid, target);
    let endpoint = raw
        .services
        .iter()
        .find(|s| s.name_any() == endpoint_name)
        .and_then(|s| s.uid())
        .map(|uid| CleanupResourceIdentity::Present {
            name: endpoint_name.clone(),
            uid,
        })
        .unwrap_or(CleanupResourceIdentity::Absent {
            name: endpoint_name,
        });
    vec![(
        target.clone(),
        ReplicaCleanupIdentity {
            pod: CleanupResourceIdentity::Present {
                name: pod_name,
                uid: target.instance_id.to_string(),
            },
            pvc: CleanupResourceIdentity::Present {
                name: pvc_name,
                uid: pvc.uid().expect("proven UID"),
            },
            endpoint,
        },
        false,
    )]
}

fn mounted_pvc(pod: &Pod) -> Option<&str> {
    let mut claims = pod.spec.as_ref()?.volumes.as_ref()?.iter().filter_map(|v| {
        v.persistent_volume_claim
            .as_ref()
            .map(|p| p.claim_name.as_str())
    });
    let name = claims.next()?;
    claims.next().is_none().then_some(name)
}

pub(crate) fn name(identity: &CleanupResourceIdentity) -> &str {
    match identity {
        CleanupResourceIdentity::Present { name, .. }
        | CleanupResourceIdentity::Absent { name } => name,
    }
}

pub(crate) fn protected(
    raw: &RawObservation,
    resource_name: Option<&str>,
    resource_uid: Option<&str>,
) -> bool {
    let Some(status) = raw.set.status.as_ref().map(|s| &s.authority) else {
        return false;
    };
    status.transition.iter().filter_map(|t| t.secondary_scale_down.as_ref())
        .chain(status.secondary_scale_down_cleanup.iter().map(|c| &c.evidence.preparation.intent))
        .chain(status.last_secondary_removal.iter().map(|r| &r.evidence.preparation.intent))
        .flat_map(|intent| [&intent.cleanup.pod, &intent.cleanup.pvc, &intent.cleanup.endpoint])
        .any(|identity| resource_name == Some(name(identity))
            || matches!(identity, CleanupResourceIdentity::Present { uid, .. } if resource_uid == Some(uid.as_str())))
}

pub(crate) fn matches<K: ResourceExt>(identity: &CleanupResourceIdentity, object: &K) -> bool {
    matches!(identity, CleanupResourceIdentity::Present { name, uid }
        if object.name_any() == *name && object.uid().as_deref() == Some(uid.as_str()))
}

pub(crate) fn finish(raw: &mut RawObservation, mut exact: RawScaleDownResources, frozen: bool) {
    if !frozen && let ExactLookup::Present(service) = &exact.endpoint {
        let owned = service.labels().get(SET_UID_LABEL) == raw.set.metadata.uid.as_ref()
            && service
                .spec
                .as_ref()
                .and_then(|s| s.selector.as_ref())
                .and_then(|s| s.get(INSTANCE_LABEL))
                .map(String::as_str)
                == Some(exact.target.instance_id.as_str());
        if !owned {
            exact.endpoint =
                ExactLookup::Failed("endpoint is not owned by the exact accepted target".into());
        } else if matches!(
            exact.identity.endpoint,
            CleanupResourceIdentity::Absent { .. }
        ) && let Some(uid) = service.uid()
        {
            exact.identity.endpoint = CleanupResourceIdentity::Present {
                name: service.name_any(),
                uid,
            };
        }
    }
    for (kind, identity, result) in [
        (
            "Pod",
            &exact.identity.pod,
            classify(&exact.identity.pod, &exact.pod),
        ),
        (
            "PVC",
            &exact.identity.pvc,
            classify(&exact.identity.pvc, &exact.pvc),
        ),
        (
            "Service",
            &exact.identity.endpoint,
            classify(&exact.identity.endpoint, &exact.endpoint),
        ),
    ] {
        if let ExactResourceObservation::LookupFailed { message } = result {
            raw.failures.push(RawObservationFailure {
                source: format!("exact-{kind}/{}", name(identity)),
                message,
            });
        }
    }
    merge(&mut raw.pods, name(&exact.identity.pod), &exact.pod);
    merge(&mut raw.pvcs, name(&exact.identity.pvc), &exact.pvc);
    merge(
        &mut raw.services,
        name(&exact.identity.endpoint),
        &exact.endpoint,
    );
    raw.exact_resources.push(exact);
}

fn merge<K: ResourceExt + Clone>(objects: &mut Vec<K>, name: &str, lookup: &ExactLookup<K>) {
    if !matches!(lookup, ExactLookup::Failed(_)) {
        objects.retain(|old| old.name_any() != name);
    }
    if let ExactLookup::Present(object) = lookup {
        objects.push(object.clone());
    }
}

pub(crate) fn classify<K: ResourceExt>(
    identity: &CleanupResourceIdentity,
    lookup: &ExactLookup<K>,
) -> ExactResourceObservation {
    match lookup {
        ExactLookup::NotFound => ExactResourceObservation::NotFound,
        ExactLookup::Failed(message) => ExactResourceObservation::LookupFailed {
            message: message.clone(),
        },
        ExactLookup::Present(object) => {
            let Some((uid, resource_version)) = object
                .uid()
                .filter(|s| !s.is_empty())
                .zip(object.resource_version().filter(|s| !s.is_empty()))
            else {
                return ExactResourceObservation::LookupFailed {
                    message: "exact GET omitted UID or resourceVersion".into(),
                };
            };
            if object.name_any() != name(identity) {
                return ExactResourceObservation::LookupFailed {
                    message: "exact GET returned another name".into(),
                };
            }
            if matches(identity, object) {
                ExactResourceObservation::FrozenUidPresent { resource_version }
            } else {
                ExactResourceObservation::ReplacementPresent {
                    uid,
                    resource_version,
                }
            }
        }
    }
}

pub(crate) fn normalized(
    raw: &RawObservation,
    resource_uid: &ResourceUid,
) -> Vec<SecondaryScaleDownResourceObservation> {
    raw.exact_resources
        .iter()
        .map(|r| SecondaryScaleDownResourceObservation {
            resource_uid: resource_uid.clone(),
            target: r.target.clone(),
            identity: r.identity.clone(),
            pod: classify(&r.identity.pod, &r.pod),
            pvc: classify(&r.identity.pvc, &r.pvc),
            endpoint: classify(&r.identity.endpoint, &r.endpoint),
        })
        .collect()
}

pub(crate) fn frozen_pod_target<'a>(
    raw: &'a RawObservation,
    pod: &Pod,
) -> Option<&'a ReplicaIdentity> {
    raw.exact_resources
        .iter()
        .find(|r| matches(&r.identity.pod, pod))
        .map(|r| &r.target)
}

pub(crate) fn frozen_pvc_target<'a>(
    raw: &'a RawObservation,
    pvc: &PersistentVolumeClaim,
) -> Option<&'a ReplicaIdentity> {
    raw.exact_resources
        .iter()
        .find(|r| matches(&r.identity.pvc, pvc))
        .map(|r| &r.target)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_reads_supersede_stale_lists_without_adopting_replacements() {
        let pod = Pod {
            metadata: kube::core::ObjectMeta {
                name: Some("frozen".into()),
                uid: Some("old".into()),
                resource_version: Some("10".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let identity = CleanupResourceIdentity::Present {
            name: "frozen".into(),
            uid: "old".into(),
        };
        let mut pods = vec![pod.clone()];
        let failed = ExactLookup::Failed("403".into());
        merge(&mut pods, "frozen", &failed);
        assert_eq!(pods.as_slice(), std::slice::from_ref(&pod));
        assert!(matches!(
            classify(&identity, &failed),
            ExactResourceObservation::LookupFailed { .. }
        ));
        merge(&mut pods, "frozen", &ExactLookup::NotFound);
        assert!(pods.is_empty());
        let mut replacement = pod;
        replacement.metadata.uid = Some("new".into());
        let observed = ExactLookup::Present(replacement.clone());
        merge(&mut pods, "frozen", &observed);
        assert_eq!(pods, [replacement]);
        assert_eq!(
            classify(&identity, &observed),
            ExactResourceObservation::ReplacementPresent {
                uid: "new".into(),
                resource_version: "10".into(),
            }
        );
    }
}
