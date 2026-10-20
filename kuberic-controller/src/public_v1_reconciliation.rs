use crate::cluster_api::PublicV1EvidenceApi;
use crate::evaluator::public_v1_evidence::{
    PublicV1EvidencePlan, plan_persisted_public_v1_evidence, plan_public_v1_evidence,
};
use kuberic_runtime::protocol::public_operations::PublicV1AgentReport;

pub(crate) fn renewal_settlement_id(
    renewal: &kuberic_runtime::protocol::types::OperationId,
    replica: kuberic_runtime::protocol::types::ReplicaId,
    revision: u64,
    program: &kuberic_runtime::protocol::public_operations::PublicOperationProgram,
) -> kuberic_runtime::protocol::types::OperationId {
    kuberic_runtime::protocol::types::OperationId::new(format!(
        "{renewal}-settle-{replica}-{revision}-{}",
        program.digest().sha256()
    ))
}

pub(crate) fn ordinary_recovery_configuration<'a>(
    observation: &crate::observation::RawObservation,
    expected: &crate::evaluator::public_v1_evidence::PublicV1AcceptedTransitionStatus,
    command: &'a kuberic_runtime::protocol::command::PublicV1OperationCommand,
) -> Result<Option<&'a kuberic_runtime::protocol::public_operations::PublicConfiguration>, String> {
    use kuberic_runtime::protocol::public_operations::{PublicCatchUpMode, PublicOperationProgram};
    use kuberic_runtime::protocol::types::ReplicaRole;
    let Some(topology) = observation
        .set
        .status
        .as_ref()
        .and_then(|status| status.public_v1_topology.as_ref())
    else {
        return Ok(None);
    };
    let Some(PublicOperationProgram::CatchUp {
        configuration,
        mode: PublicCatchUpMode::All,
    }) = command.intent.program.as_ref()
    else {
        return Ok(None);
    };
    if topology.pending_switchover.is_some()
        || expected.intent.consumer
            != crate::evaluator::public_v1_evidence::PublicV1EvidenceConsumer::Topology
        || configuration.previous.is_some()
        || configuration.current != topology.configuration()?
        || !topology.sessions.iter().any(|session| {
            session.role == ReplicaRole::Primary
                && session.identity == command.target
                && session.storage_incarnation == expected.intent.source.storage_incarnation
                && session.process_session_id == command.intent.process_session_id
        })
    {
        return Ok(None);
    }
    Ok(Some(configuration))
}

pub(crate) fn recovery_peer_changed(
    observation: &crate::observation::RawObservation,
    configuration: &kuberic_runtime::protocol::public_operations::PublicConfiguration,
    expected: &crate::evaluator::public_v1_evidence::PublicV1AcceptedTransitionStatus,
) -> Result<bool, String> {
    use crate::observation::RawAgentObservation;
    use kube::ResourceExt;
    let source = &expected.intent.source.storage_incarnation.replica;
    for peer in &configuration.replicas {
        if peer.identity == *source {
            continue;
        }
        if !observation.pods.iter().any(|pod| {
            pod.uid().as_deref() == Some(peer.identity.instance_id.as_str())
                && pod.metadata.deletion_timestamp.is_none()
        }) {
            return Ok(true);
        }
        let key = kuberic_runtime::protocol::observation::ReplicaObservationKey::new(
            peer.identity.replica_id,
            peer.identity.instance_id.clone(),
        );
        match observation.agents.get(&key) {
            Some(RawAgentObservation::PublicV1Report(report)) => {
                report.validate().map_err(str::to_owned)?;
                if report.replica != peer.identity
                    || report.identity != expected.evidence.identity
                    || report.history != expected.intent.history
                    || peer.storage_incarnation.as_ref() != Some(&report.storage_incarnation)
                {
                    return Err("ordinary recovery peer changed exact durable authority".into());
                }
                let admitted = observation
                    .set
                    .status
                    .as_ref()
                    .and_then(|status| status.public_v1_topology.as_ref())
                    .and_then(|topology| {
                        topology.sessions.iter().find(|session| {
                            session.identity == peer.identity
                                && session.process_session_id == peer.process_session_id
                        })
                    });
                let session_changed = report.process_session_id != peer.process_session_id;
                if let Some(admitted) = admitted {
                    let creation_changed =
                        report.replicator_creation_id != admitted.replicator_creation_id;
                    if session_changed != creation_changed {
                        return Err(
                            "ordinary recovery peer changed only one session component".into()
                        );
                    }
                }
                if session_changed {
                    return Ok(true);
                }
            }
            Some(RawAgentObservation::Invalid { message }) => return Err(message.clone()),
            Some(RawAgentObservation::Uninitialized(_)) => {
                return Err("ordinary recovery peer lost initialized authority".into());
            }
            Some(RawAgentObservation::Absent | RawAgentObservation::Unavailable { .. }) | None => {}
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicV1ReconciliationOutcome {
    Waiting,
    EvidencePersisted,
    EffectApplied,
}

fn outcome(plan: &PublicV1EvidencePlan) -> PublicV1ReconciliationOutcome {
    match plan {
        PublicV1EvidencePlan::Wait => PublicV1ReconciliationOutcome::Waiting,
        PublicV1EvidencePlan::PersistStatus { .. } => {
            PublicV1ReconciliationOutcome::EvidencePersisted
        }
        PublicV1EvidencePlan::Accept { .. }
        | PublicV1EvidencePlan::RetireExactIncarnation { .. }
        | PublicV1EvidencePlan::ProvisionReplacement { .. } => {
            PublicV1ReconciliationOutcome::EffectApplied
        }
    }
}

pub async fn reconcile_public_v1_consumer<A>(
    api: &A,
    scope: &crate::cluster_api::PublicV1Scope,
    report: &PublicV1AgentReport,
) -> Result<PublicV1ReconciliationOutcome, String>
where
    A: PublicV1EvidenceApi + ?Sized,
{
    let accepted = api
        .observe_public_v1_evidence_status(scope)
        .await?
        .ok_or_else(|| "public-v1 consumer lacks accepted transition status".to_string())?;
    let plan = plan_public_v1_evidence(&accepted, report)?;
    let outcome = outcome(&plan);
    crate::executor::execute_public_v1_evidence(api, scope, plan).await?;
    Ok(outcome)
}

pub async fn reconcile_public_v1_operation<A>(
    api: &A,
    scope: &crate::cluster_api::PublicV1Scope,
    command: kuberic_runtime::protocol::command::ProtocolCommand,
) -> Result<PublicV1ReconciliationOutcome, String>
where
    A: crate::cluster_api::PublicV1OperationApi + PublicV1EvidenceApi + ?Sized,
{
    let expected = api
        .observe_public_v1_operation(scope)
        .await?
        .ok_or_else(|| "public-v1 operation lacks accepted transition status".to_string())?;
    if let Some(plan) = plan_persisted_public_v1_evidence(&expected)? {
        let outcome = outcome(&plan);
        crate::executor::execute_public_v1_evidence(api, scope, plan).await?;
        return Ok(outcome);
    }
    if let Ok(report) = api
        .observe_public_v1_operation_report(scope, &expected)
        .await
    {
        let exact_completion = report
            .completed_operations
            .iter()
            .any(|completion| expected.intent.matches(completion));
        if report.process_session_id != expected.intent.process_session_id && !exact_completion {
            api.supersede_public_v1_operation(scope, &expected, &command)
                .await?;
            return Ok(PublicV1ReconciliationOutcome::EffectApplied);
        }
        let plan = plan_public_v1_evidence(&expected, &report)?;
        if !matches!(plan, PublicV1EvidencePlan::Wait) {
            return reconcile_public_v1_consumer(api, scope, &report).await;
        }
    } else if !api
        .public_v1_operation_source_present(scope, &expected)
        .await?
    {
        api.supersede_public_v1_operation(scope, &expected, &command)
            .await?;
        return Ok(PublicV1ReconciliationOutcome::EffectApplied);
    }
    if let Err(dispatch_error) =
        crate::executor::execute_public_v1_operation(api, scope, &expected, command.clone()).await
    {
        if let Ok(report) = api
            .observe_public_v1_operation_report(scope, &expected)
            .await
        {
            let outcome = reconcile_public_v1_consumer(api, scope, &report).await?;
            if outcome != PublicV1ReconciliationOutcome::Waiting {
                return Ok(outcome);
            }
        }
        // A failed RPC observer does not retract an admitted native operation.
        // Supersession/retirement requires its own exact authority; preserve
        // the receiver while an idempotent retry observes the selected work.
        tracing::warn!(
            operation = %expected.intent.operation_id,
            error = %dispatch_error,
            "public operation dispatch failed; retained work is not cancelled"
        );
        return Err(dispatch_error);
    }
    let report = api
        .observe_public_v1_operation_report(scope, &expected)
        .await?;
    reconcile_public_v1_consumer(api, scope, &report).await
}
