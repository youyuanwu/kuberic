use super::*;
use crate::command::ScaleDownResource;
use crate::types::ReplacementCleanup;

pub(super) fn freeze(
    snapshot: &ObservationSnapshot,
    target: &ReplicaIdentity,
) -> Option<ReplacementCleanup> {
    let exact = secondary_scale_down::resources(snapshot, target)?;
    if ![
        (&exact.identity.pod, &exact.pod),
        (&exact.identity.pvc, &exact.pvc),
        (&exact.identity.endpoint, &exact.endpoint),
    ]
    .iter()
    .all(|(identity, observed)| secondary_scale_down::observed(identity, observed))
    {
        return None;
    }
    Some(ReplacementCleanup {
        resource_uid: snapshot.resource_uid.clone(),
        target: target.clone(),
        resources: exact.identity.clone(),
    })
}

pub(super) fn waiting(snapshot: &ObservationSnapshot, config: &EvaluationConfig) -> Plan {
    Plan::Wait {
        reason: WaitReason::ActiveTransition,
        status: waiting_status(
            snapshot.status.clone(),
            "ReplacementCleanupPending",
            "Exact replacement cleanup must finish before another operation can start",
        ),
        requeue_after_seconds: config.wait_requeue_seconds,
    }
}

pub(super) fn evaluate(
    snapshot: &ObservationSnapshot,
    cleanup: &ReplacementCleanup,
    config: &EvaluationConfig,
) -> Plan {
    let Some(exact) = secondary_scale_down::resources(snapshot, &cleanup.target)
        .filter(|r| r.identity == cleanup.resources && r.resource_uid == cleanup.resource_uid)
    else {
        return waiting(snapshot, config);
    };
    for (kind, identity, observation) in [
        (
            ScaleDownResource::Endpoint,
            &cleanup.resources.endpoint,
            &exact.endpoint,
        ),
        (ScaleDownResource::Pod, &cleanup.resources.pod, &exact.pod),
        (ScaleDownResource::Pvc, &cleanup.resources.pvc, &exact.pvc),
    ] {
        if secondary_scale_down::absent(identity, observation) {
            continue;
        }
        return match secondary_scale_down::delete(kind, identity, observation) {
            Some(change) => Plan::Apply {
                changes: vec![change],
            },
            None => waiting(snapshot, config),
        };
    }
    let mut status = snapshot.status.clone();
    status.last_replacement = None;
    Plan::Apply {
        changes: vec![KubernetesChange::PersistStatus {
            status: Box::new(waiting_status(
                status,
                "ReplacementCleanupComplete",
                "Frozen replacement resources are absent; re-observe before admitting new work",
            )),
        }],
    }
}
