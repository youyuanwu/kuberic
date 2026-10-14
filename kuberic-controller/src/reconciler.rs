use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::evaluator::EvaluationConfig;
use kube::ResourceExt;
use kuberic_runtime::protocol::types::ResourceUid;
use tokio::sync::Mutex;

use crate::cluster_api::{ProductionClusterApi, PublicV1Scope};
use crate::evaluator::public_v1_history::{
    PublicV1HistoryPlan, plan_bootstrap_history, plan_recovery_report,
};
use crate::evaluator::public_v1_session_renewal::plan_session_renewal;
use crate::executor::ExecutionKind;
use crate::observation::RawAgentObservation;
use crate::{ControllerError, Result};

pub struct Reconciler {
    api: Arc<dyn ProductionClusterApi>,
    evaluation: EvaluationConfig,
    locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
}

impl Reconciler {
    pub fn new(api: Arc<dyn ProductionClusterApi>, evaluation: EvaluationConfig) -> Self {
        Self {
            api,
            evaluation,
            locks: Mutex::new(BTreeMap::new()),
        }
    }

    pub async fn reconcile(&self, namespace: &str, name: &str) -> Result<ReconcileAction> {
        let key = format!("{namespace}/{name}");
        let lock = {
            let mut locks = self.locks.lock().await;
            locks
                .entry(key.clone())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        let _guard = lock.lock().await;
        let raw = self.api.observe(namespace, name).await?;
        let resource_uid = raw
            .set
            .uid()
            .map(ResourceUid::new)
            .ok_or_else(|| ControllerError::Observation("KubericSet has no UID".into()))?;
        let scope = PublicV1Scope {
            namespace: namespace.to_string(),
            name: name.to_string(),
            resource_uid: resource_uid.clone(),
        };
        let accepted = self
            .api
            .observe_public_v1_status(&scope)
            .await
            .map_err(ControllerError::InvalidAgentEvidence)?;
        let reports = raw
            .agents
            .values()
            .filter_map(|observation| match observation {
                RawAgentObservation::PublicV1Report(report) => Some((**report).clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let status = raw.set.status.as_ref();
        if let Some(renewal) = status
            .and_then(|status| status.public_v1_session_renewal.as_ref())
            .map(crate::crd::PublicV1SessionRenewalState::renewal)
            .transpose()
            .map_err(ControllerError::InvalidAgentEvidence)?
        {
            let unreachable = Vec::new();
            let plan = plan_session_renewal(&renewal, &unreachable)
                .map_err(ControllerError::InvalidAgentEvidence)?;
            crate::executor::execute_public_v1_session_renewal(self.api.as_ref(), &scope, plan)
                .await
                .map_err(ControllerError::InvalidAgentEvidence)?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Executed,
            });
        }
        if status
            .and_then(|status| status.public_v1_transition.as_ref())
            .is_some()
        {
            let command = status
                .and_then(|status| status.public_v1_operation.clone())
                .ok_or_else(|| {
                    ControllerError::InvalidAgentEvidence(
                        "public-v1 transition lacks its operation command".into(),
                    )
                })?
                .command()
                .map_err(ControllerError::InvalidAgentEvidence)?;
            crate::public_v1_reconciliation::reconcile_public_v1_operation(
                self.api.as_ref(),
                &scope,
                kuberic_runtime::protocol::command::ProtocolCommand::PublicV1Operation(Box::new(
                    command,
                )),
            )
            .await
            .map_err(ControllerError::InvalidAgentEvidence)?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Executed,
            });
        }
        let Some(report) = reports
            .iter()
            .find(|report| report.resource_uid == resource_uid)
        else {
            return Ok(ReconcileAction {
                requeue_after: Duration::from_secs(self.evaluation.wait_requeue_seconds),
                kind: ReconcileKind::Waiting,
            });
        };
        if let Some(accepted) = accepted.as_ref()
            && report.identity == accepted.identity
            && report.history == accepted.history
            && report.data_loss_recovery.is_some()
        {
            let plan = plan_recovery_report(accepted, report)
                .map_err(ControllerError::InvalidAgentEvidence)?;
            crate::executor::execute_public_v1_history(self.api.as_ref(), &scope, plan)
                .await
                .map_err(ControllerError::InvalidAgentEvidence)?;
            return Ok(ReconcileAction {
                requeue_after: Duration::ZERO,
                kind: ReconcileKind::Applied,
            });
        }
        if accepted.as_ref().is_some_and(|accepted| {
            report.identity == accepted.identity && report.history == accepted.history
        }) {
            return Ok(ReconcileAction {
                requeue_after: Duration::from_secs(self.evaluation.stable_resync_seconds),
                kind: ReconcileKind::Stable,
            });
        }
        let identity = accepted
            .as_ref()
            .map(|accepted| accepted.identity.clone())
            .unwrap_or_else(|| report.identity.clone());
        if report.identity != identity {
            return Err(ControllerError::InvalidAgentEvidence(
                "public-v1 report identity differs from accepted history".into(),
            ));
        }
        let plan = plan_bootstrap_history(
            accepted,
            &resource_uid,
            &identity,
            report.storage_incarnation.clone(),
        )
        .map_err(ControllerError::InvalidAgentEvidence)?;
        let kind = match plan_kind_for_history(&plan) {
            ExecutionKind::Applied => ReconcileKind::Applied,
            ExecutionKind::Stable => ReconcileKind::Stable,
            _ => ReconcileKind::Executed,
        };
        crate::executor::execute_public_v1_history(self.api.as_ref(), &scope, plan)
            .await
            .map_err(ControllerError::InvalidAgentEvidence)?;
        Ok(ReconcileAction {
            requeue_after: Duration::ZERO,
            kind,
        })
    }
}

fn plan_kind_for_history(plan: &PublicV1HistoryPlan) -> ExecutionKind {
    match plan {
        PublicV1HistoryPlan::PersistStatus { .. } => ExecutionKind::Applied,
        PublicV1HistoryPlan::Dispatch { .. } => ExecutionKind::Executed,
        PublicV1HistoryPlan::Stable => ExecutionKind::Stable,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileKind {
    Stable,
    Applied,
    Executed,
    Waiting,
    Unsafe,
    ObservationStale,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileAction {
    pub requeue_after: Duration,
    pub kind: ReconcileKind,
}
