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
pub async fn execute_public_v1_authorization(
    api: &dyn crate::cluster_api::PublicV1AuthorizationApi,
    plan: crate::evaluator::test_bridge::PublicV1AuthorizationPlan,
) -> std::result::Result<(), String> {
    use crate::evaluator::public_v1_authorization::{
        dispatch_public_v1_authorization, mark_public_v1_authorization_dispatched,
        sign_public_v1_authorization,
    };
    use crate::evaluator::test_bridge::PublicV1AuthorizationPlan;

    match plan {
        PublicV1AuthorizationPlan::Stable => Ok(()),
        PublicV1AuthorizationPlan::CreateSecret { expected, owner } => {
            if api.observe_public_v1_authorization_status().await?.as_ref()
                != Some(expected.as_ref())
            {
                return Err("stale public-v1 authorization Secret creation".into());
            }
            owner.validate_for_accepted(&expected)?;
            if expected
                .authorization()
                .and_then(|authorization| authorization.secret_binding.as_ref())
                .is_some()
            {
                return Err("bound public-v1 authorization Secret cannot be regenerated".into());
            }
            if api
                .observe_public_v1_authorization_secret(&owner.secret_name())
                .await?
                .is_some()
            {
                return Err(
                    "existing public-v1 authorization Secret cannot be replaced or repaired".into(),
                );
            }
            let pods = api.observe_public_v1_authorization_pods().await?;
            crate::cluster_api::validate_pods_do_not_reference_public_v1_authorization_secret(
                &pods,
                &owner.secret_name(),
            )?;
            api.create_public_v1_authorization_secret(
                crate::cluster_api::generate_public_v1_authorization_secret(&owner)?,
            )
            .await
        }
        PublicV1AuthorizationPlan::PersistStatus {
            expected,
            next,
            owner,
        } => {
            if api.observe_public_v1_authorization_status().await?.as_ref()
                != Some(expected.as_ref())
            {
                return Err("stale public-v1 authorization status replacement".into());
            }
            owner.validate_for_accepted(&expected)?;
            owner.validate_for_accepted(&next)?;
            if !next.is_authorization_successor_of(&expected) {
                return Err("public-v1 authorization status transition is not monotonic".into());
            }
            let expected_binding = expected
                .authorization()
                .and_then(|authorization| authorization.secret_binding.as_ref());
            let next_binding = next
                .authorization()
                .and_then(|authorization| authorization.secret_binding.as_ref())
                .ok_or_else(|| {
                    "public-v1 authorization status cannot remove its Secret binding".to_string()
                })?;
            if expected_binding.is_some_and(|binding| binding != next_binding) {
                return Err(
                    "public-v1 authorization Secret binding is immutable after acceptance".into(),
                );
            }
            let secret = api
                .observe_public_v1_authorization_secret(&next_binding.name)
                .await?
                .ok_or_else(|| {
                    "bound public-v1 authorization Secret was lost; regeneration is forbidden"
                        .to_string()
                })?;
            crate::cluster_api::validate_public_v1_authorization_binding(
                &secret,
                &owner,
                next_binding,
            )?;
            let pods = api.observe_public_v1_authorization_pods().await?;
            crate::cluster_api::validate_pods_do_not_reference_public_v1_authorization_secret(
                &pods,
                &next_binding.name,
            )?;
            api.persist_public_v1_authorization_status(&expected, &next)
                .await
        }
        PublicV1AuthorizationPlan::Sign {
            expected,
            owner,
            operation_id,
        } => {
            if api.observe_public_v1_authorization_status().await?.as_ref()
                != Some(expected.as_ref())
            {
                return Err("stale public-v1 authorization signing authority".into());
            }
            owner.validate_for_accepted(&expected)?;
            let binding = expected
                .authorization()
                .and_then(|authorization| authorization.secret_binding.as_ref())
                .ok_or_else(|| {
                    "public-v1 authorization signing lacks a Secret binding".to_string()
                })?;
            let secret = api
                .observe_public_v1_authorization_secret(&binding.name)
                .await?
                .ok_or_else(|| {
                    "bound public-v1 authorization Secret was lost; regeneration is forbidden"
                        .to_string()
                })?;
            let material = crate::cluster_api::validate_public_v1_authorization_binding(
                &secret, &owner, binding,
            )?;
            let pods = api.observe_public_v1_authorization_pods().await?;
            crate::cluster_api::validate_pods_do_not_reference_public_v1_authorization_secret(
                &pods,
                &binding.name,
            )?;
            let next = sign_public_v1_authorization(&expected, &operation_id, &material)?;
            api.persist_public_v1_authorization_status(&expected, &next)
                .await
        }
        PublicV1AuthorizationPlan::Dispatch {
            expected,
            owner,
            operation_id,
        } => {
            if api.observe_public_v1_authorization_status().await?.as_ref()
                != Some(expected.as_ref())
            {
                return Err("stale public-v1 authorization dispatch authority".into());
            }
            owner.validate_for_accepted(&expected)?;
            let binding = expected
                .authorization()
                .and_then(|authorization| authorization.secret_binding.as_ref())
                .ok_or_else(|| {
                    "public-v1 authorization dispatch lacks a Secret binding".to_string()
                })?;
            let secret = api
                .observe_public_v1_authorization_secret(&binding.name)
                .await?
                .ok_or_else(|| {
                    "bound public-v1 authorization Secret was lost; regeneration is forbidden"
                        .to_string()
                })?;
            crate::cluster_api::validate_public_v1_authorization_binding(&secret, &owner, binding)?;
            let pods = api.observe_public_v1_authorization_pods().await?;
            crate::cluster_api::validate_pods_do_not_reference_public_v1_authorization_secret(
                &pods,
                &binding.name,
            )?;
            let dispatch = dispatch_public_v1_authorization(&expected, &operation_id)?;
            api.dispatch_public_v1_authorization(&expected, &dispatch)
                .await?;
            let next = mark_public_v1_authorization_dispatched(&expected, &operation_id)?;
            api.persist_public_v1_authorization_status(&expected, &next)
                .await
        }
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
