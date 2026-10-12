use std::time::Duration;

use crate::plan::Plan;
use kuberic_runtime::protocol::command::{KubernetesChange, SafetyChange};
use kuberic_runtime::protocol::observation::ObservationSnapshot;

use crate::Result;
use crate::cluster_api::ClusterApi;
use crate::observation::RawObservation;

#[cfg(feature = "runtime-test-bridge")]
pub async fn execute_preview_service_location(
    api: &dyn crate::cluster_api::PreviewServiceApi,
    plan: crate::evaluator::test_bridge::PreviewServiceLocationPlan,
) -> std::result::Result<(), String> {
    use crate::evaluator::test_bridge::PreviewServiceLocationPlan;
    match plan {
        PreviewServiceLocationPlan::Stable => Ok(()),
        PreviewServiceLocationPlan::PersistStatus { expected, next } => {
            if let crate::evaluator::test_bridge::PreviewServiceLocationStage::Published(
                projection,
            ) = &next.service_location_projection
            {
                let (status, service) = api.observe_preview().await?;
                if status != expected
                    || service.metadata.uid.as_ref() != Some(&projection.service_uid)
                    || service.metadata.resource_version.as_ref()
                        != Some(&projection.service_resource_version)
                    || !crate::cluster_api::preview_service_matches(&service, projection)
                {
                    return Err("stale service-location publication".into());
                }
            }
            api.persist_preview_status(&expected, &next).await
        }
        PreviewServiceLocationPlan::WriteService {
            expected_status,
            expected_uid,
            expected_version,
            projection,
        } => {
            let (status, service) = api.observe_preview().await?;
            if status != expected_status
                || service.metadata.uid.as_ref() != Some(&expected_uid)
                || service.metadata.resource_version.as_ref() != Some(&expected_version)
            {
                return Err("stale service-location write".into());
            }
            if expected_status.service_location_projection
                != crate::evaluator::test_bridge::PreviewServiceLocationStage::Pending(
                    projection.clone(),
                )
            {
                return Err(
                    "service-location mutation is not the accepted pending projection".into(),
                );
            }
            api.replace_preview_service(crate::cluster_api::preview_service_update(
                &service,
                &projection,
            ))
            .await
        }
    }
}

#[cfg(feature = "runtime-test-bridge")]
pub async fn execute_public_v1_history(
    api: &dyn crate::cluster_api::PublicV1HistoryApi,
    plan: crate::evaluator::test_bridge::PublicV1HistoryPlan,
) -> std::result::Result<(), String> {
    use crate::evaluator::test_bridge::PublicV1HistoryPlan;
    match plan {
        PublicV1HistoryPlan::Stable => Ok(()),
        PublicV1HistoryPlan::PersistStatus { expected, next } => {
            if api.observe_public_v1_status().await? != expected {
                return Err("stale public-v1 accepted status".into());
            }
            api.persist_public_v1_status(expected.as_ref(), &next).await
        }
        PublicV1HistoryPlan::Dispatch { expected, command } => {
            if api.observe_public_v1_status().await?.as_ref() != Some(&expected) {
                return Err("stale public-v1 dispatch authority".into());
            }
            let kuberic_runtime::protocol::command::ProtocolCommand::PublicV1History(
                public_command,
            ) = &command
            else {
                return Err("public-v1 history executor received another command".into());
            };
            if public_command.accepted() != Some(&expected) {
                return Err("public-v1 command differs from accepted status".into());
            }
            api.dispatch_public_v1_history(&expected, &command).await
        }
    }
}

#[cfg(feature = "runtime-test-bridge")]
pub async fn execute_public_v1_session_renewal(
    api: &dyn crate::cluster_api::PublicV1SessionRenewalApi,
    plan: crate::evaluator::test_bridge::PublicV1SessionRenewalPlan,
) -> std::result::Result<(), String> {
    use crate::evaluator::test_bridge::PublicV1SessionRenewalPlan;
    match plan {
        PublicV1SessionRenewalPlan::Stable | PublicV1SessionRenewalPlan::WaitForSurvivor { .. } => {
            Ok(())
        }
        PublicV1SessionRenewalPlan::PersistStatus { expected, next } => {
            if api.observe_public_v1_session_renewal().await?.as_ref() != expected.as_deref() {
                return Err("stale public-v1 session-renewal status".into());
            }
            api.persist_public_v1_session_renewal(expected.as_deref(), &next)
                .await
        }
        PublicV1SessionRenewalPlan::Dispatch { expected, command } => {
            if api.observe_public_v1_session_renewal().await?.as_ref() != Some(expected.as_ref()) {
                return Err("stale public-v1 session-renewal dispatch authority".into());
            }
            let kuberic_runtime::protocol::command::ProtocolCommand::PublicV1History(
                public_command,
            ) = &command
            else {
                return Err("public-v1 session-renewal executor received another command".into());
            };
            let kuberic_runtime::protocol::command::PublicV1HistoryCommand::SessionRenewal(
                renewal_command,
            ) = public_command.as_ref()
            else {
                return Err("public-v1 session-renewal executor received a history command".into());
            };
            renewal_command.validate().map_err(str::to_owned)?;
            if renewal_command.accepted() != expected.as_ref() {
                return Err(
                    "public-v1 session-renewal command differs from accepted status".into(),
                );
            }
            api.dispatch_public_v1_session_renewal(&expected, &command)
                .await
        }
    }
}

#[cfg(feature = "runtime-test-bridge")]
pub async fn execute_public_v1_evidence(
    api: &dyn crate::cluster_api::PublicV1EvidenceApi,
    plan: crate::evaluator::test_bridge::PublicV1EvidencePlan,
) -> std::result::Result<(), String> {
    use crate::evaluator::test_bridge::PublicV1EvidencePlan;
    match plan {
        PublicV1EvidencePlan::PersistStatus { expected, next } => {
            if api.observe_public_v1_evidence_status().await?.as_ref() != Some(expected.as_ref()) {
                return Err("stale public-v1 evidence status".into());
            }
            if !next.is_evidence_successor_of(&expected) {
                return Err("public-v1 evidence status transition is not monotonic".into());
            }
            api.persist_public_v1_evidence_status(&expected, &next)
                .await
        }
        PublicV1EvidencePlan::Accept {
            expected,
            completion,
        } => {
            if api.observe_public_v1_evidence_status().await?.as_ref() != Some(expected.as_ref()) {
                return Err("stale public-v1 completion acceptance".into());
            }
            api.accept_public_v1_completion(&expected, &completion)
                .await
        }
        PublicV1EvidencePlan::RetireExactIncarnation {
            expected,
            completion,
            storage,
        } => {
            if api.observe_public_v1_evidence_status().await?.as_ref() != Some(expected.as_ref()) {
                return Err("stale public-v1 retirement authority".into());
            }
            api.retire_public_v1_incarnation(&expected, &completion, &storage)
                .await
        }
        PublicV1EvidencePlan::ProvisionReplacement {
            expected,
            completion,
            rebuild,
        } => {
            if api.observe_public_v1_evidence_status().await?.as_ref() != Some(expected.as_ref()) {
                return Err("stale public-v1 replacement authority".into());
            }
            api.provision_public_v1_replacement(&expected, &completion, &rebuild)
                .await
        }
        PublicV1EvidencePlan::Wait => Ok(()),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutionKind {
    Stable,
    Applied,
    Executed,
    Waiting,
    Unsafe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionOutcome {
    pub kind: ExecutionKind,
    pub requeue_after: Duration,
}

pub async fn execute_plan(
    api: &dyn ClusterApi,
    observation: &RawObservation,
    snapshot: &ObservationSnapshot,
    plan: Plan,
) -> Result<ExecutionOutcome> {
    match plan {
        Plan::Stable {
            status,
            requeue_after_seconds,
        } => {
            persist_if_changed(api, observation, snapshot, &status).await?;
            Ok(ExecutionOutcome {
                kind: ExecutionKind::Stable,
                requeue_after: Duration::from_secs(requeue_after_seconds),
            })
        }
        Plan::Apply { changes } => {
            for change in changes {
                execute_change(api, observation, change).await?;
            }
            Ok(ExecutionOutcome {
                kind: ExecutionKind::Applied,
                requeue_after: Duration::ZERO,
            })
        }
        Plan::Execute { command } => {
            api.execute_command(observation, &command).await?;
            Ok(ExecutionOutcome {
                kind: ExecutionKind::Executed,
                requeue_after: Duration::ZERO,
            })
        }
        Plan::Wait {
            status,
            requeue_after_seconds,
            ..
        } => {
            persist_if_changed(api, observation, snapshot, &status).await?;
            Ok(ExecutionOutcome {
                kind: ExecutionKind::Waiting,
                requeue_after: Duration::from_secs(requeue_after_seconds),
            })
        }
        Plan::Unsafe {
            status,
            safety_changes,
            requeue_after_seconds,
            ..
        } => {
            for change in safety_changes {
                match change {
                    SafetyChange::RemoveWriteRouting => {
                        api.remove_write_routing(observation).await?;
                    }
                }
            }
            persist_if_changed(api, observation, snapshot, &status).await?;
            Ok(ExecutionOutcome {
                kind: ExecutionKind::Unsafe,
                requeue_after: Duration::from_secs(requeue_after_seconds),
            })
        }
    }
}

async fn execute_change(
    api: &dyn ClusterApi,
    observation: &RawObservation,
    change: KubernetesChange,
) -> Result<()> {
    match change {
        KubernetesChange::DeleteScaleDownResource {
            resource,
            name,
            uid,
            resource_version,
        } => {
            api.delete_scale_down_resource(observation, resource, &name, &uid, &resource_version)
                .await
        }
        KubernetesChange::EnsureReplicaSupport => api.ensure_replica_support(observation).await,
        KubernetesChange::EnsureReplicaScaffolding { replica_ids } => {
            api.ensure_replica_scaffolding(observation, &replica_ids)
                .await
        }
        KubernetesChange::EnsureReplacementScaffolding {
            replica_id,
            replacing,
        } => {
            api.ensure_replacement_scaffolding(observation, replica_id, &replacing)
                .await
        }
        KubernetesChange::DeleteReplicaScaffolding {
            pod_name,
            pod_uid,
            pvc_name,
            pvc_uid,
        } => {
            api.delete_replica_scaffolding(
                observation,
                pod_name.as_deref(),
                pod_uid.as_ref(),
                pvc_name.as_deref(),
                pvc_uid.as_ref(),
            )
            .await
        }
        KubernetesChange::DeleteReplicaEndpoint { identity } => {
            api.delete_replica_endpoint(observation, &identity).await
        }
        KubernetesChange::DeleteExactPod { pod_name, pod_uid } => {
            api.delete_exact_pod(observation, &pod_name, &pod_uid).await
        }
        KubernetesChange::DeleteExactService {
            name,
            uid,
            resource_version,
        } => {
            api.delete_exact_service(observation, &name, &uid, &resource_version)
                .await
        }
        KubernetesChange::EnsureWriteRoutingService => {
            api.ensure_write_routing_service(observation).await
        }
        KubernetesChange::PersistStatus { status } => {
            api.replace_status(observation, &status).await
        }
        KubernetesChange::RemoveWriteRouting => api.remove_write_routing(observation).await,
        KubernetesChange::PublishWriteRouting { primary } => {
            api.publish_write_routing(observation, &primary).await
        }
    }
}

async fn persist_if_changed(
    api: &dyn ClusterApi,
    observation: &RawObservation,
    snapshot: &ObservationSnapshot,
    status: &kuberic_runtime::protocol::types::AcceptedStatus,
) -> Result<()> {
    if &snapshot.status != status {
        api.replace_status(observation, status).await?;
    }
    Ok(())
}
