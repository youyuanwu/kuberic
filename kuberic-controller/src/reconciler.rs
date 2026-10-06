use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use crate::evaluator::{EvaluationConfig, evaluate};
use kuberic_runtime::protocol::observation::{ReplicaObservationKey, ReportWatermark};
use tokio::sync::Mutex;

use crate::cluster_api::ClusterApi;
use crate::executor::{ExecutionKind, ExecutionOutcome, execute_plan};
use crate::normalize::{normalize, report_watermarks};
use crate::plan::Plan;
use crate::{ControllerError, Result};
use kuberic_runtime::protocol::command::KubernetesChange;
use kuberic_runtime::protocol::types::AcceptedStatus;

type Watermarks = BTreeMap<ReplicaObservationKey, ReportWatermark>;

pub struct Reconciler {
    api: Arc<dyn ClusterApi>,
    evaluation: EvaluationConfig,
    locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    watermarks: Mutex<BTreeMap<String, Watermarks>>,
}

impl Reconciler {
    pub fn new(api: Arc<dyn ClusterApi>, evaluation: EvaluationConfig) -> Self {
        Self {
            api,
            evaluation,
            locks: Mutex::new(BTreeMap::new()),
            watermarks: Mutex::new(BTreeMap::new()),
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
        let previous = self
            .watermarks
            .lock()
            .await
            .get(&key)
            .cloned()
            .unwrap_or_default();
        let snapshot = normalize(raw.clone(), previous.clone())?;
        let next_watermarks = merge_watermarks(previous, report_watermarks(&snapshot));
        self.watermarks
            .lock()
            .await
            .insert(key.clone(), next_watermarks);
        let plan = project_placement_conditions(&raw, evaluate(&snapshot, &self.evaluation));
        if plan_allows_auto_balance(&plan, &snapshot.status)
            && let Some(request) = crate::primary_balancing::plan_primary_balance(&raw, &snapshot)
        {
            match self.api.patch_switchover(&raw, &request).await {
                Ok(()) => {
                    return Ok(ReconcileAction {
                        requeue_after: Duration::ZERO,
                        kind: ReconcileKind::Applied,
                    });
                }
                Err(ControllerError::ObservationStale) => {
                    return Ok(ReconcileAction {
                        requeue_after: Duration::ZERO,
                        kind: ReconcileKind::ObservationStale,
                    });
                }
                Err(error) => return Err(error),
            }
        }
        let outcome = match execute_plan(self.api.as_ref(), &raw, &snapshot, plan).await {
            Ok(outcome) => outcome,
            Err(ControllerError::ObservationStale) => {
                return Ok(ReconcileAction {
                    requeue_after: Duration::ZERO,
                    kind: ReconcileKind::ObservationStale,
                });
            }
            Err(ControllerError::AgentUnavailable(message)) => {
                if snapshot
                    .status
                    .transition
                    .as_ref()
                    .is_some_and(|transition| {
                        transition.kind
                            == kuberic_runtime::protocol::types::TransitionKind::PlannedSwitchover
                    })
                {
                    let status = snapshot.status.clone().with_condition(kuberic_runtime::protocol::types::StatusCondition {
                        type_: "Progressing".to_string(),
                        status: kuberic_runtime::protocol::types::ConditionStatus::True,
                        reason: "SwitchoverReobservationRequired".to_string(),
                        message: format!("Re-observe exact process-session authority; dispatch is not completion evidence: {message}"),
                    });
                    match self.api.replace_status(&raw, &status).await {
                        Ok(()) => {}
                        Err(ControllerError::ObservationStale) => {
                            return Ok(ReconcileAction {
                                requeue_after: Duration::ZERO,
                                kind: ReconcileKind::ObservationStale,
                            });
                        }
                        Err(error) => return Err(error),
                    }
                }
                return Ok(ReconcileAction {
                    requeue_after: Duration::from_secs(self.evaluation.wait_requeue_seconds),
                    kind: ReconcileKind::Waiting,
                });
            }
            Err(error) => return Err(error),
        };
        Ok(action(outcome))
    }
}

fn project_placement_conditions(raw: &crate::observation::RawObservation, plan: Plan) -> Plan {
    match plan {
        Plan::Stable {
            status,
            requeue_after_seconds,
        } => Plan::Stable {
            status: crate::placement::project_conditions(raw, status),
            requeue_after_seconds,
        },
        Plan::Wait {
            reason,
            status,
            requeue_after_seconds,
        } => Plan::Wait {
            reason,
            status: crate::placement::project_conditions(raw, status),
            requeue_after_seconds,
        },
        Plan::Unsafe {
            reason,
            status,
            safety_changes,
            requeue_after_seconds,
        } => Plan::Unsafe {
            reason,
            status: crate::placement::project_conditions(raw, status),
            safety_changes,
            requeue_after_seconds,
        },
        Plan::Apply { mut changes } => {
            for change in &mut changes {
                if let KubernetesChange::PersistStatus { status } = change {
                    **status = crate::placement::project_conditions(raw, (**status).clone());
                }
            }
            Plan::Apply { changes }
        }
        Plan::Execute { command } => Plan::Execute { command },
    }
}

fn plan_allows_auto_balance(plan: &Plan, current: &AcceptedStatus) -> bool {
    matches!(plan, Plan::Stable { status, .. } if status == current)
}

fn merge_watermarks(mut previous: Watermarks, observed: Watermarks) -> Watermarks {
    for (key, watermark) in observed {
        let replace = previous.get(&key).is_none_or(|existing| {
            existing.process_session_id != watermark.process_session_id
                || watermark.report_sequence > existing.report_sequence
        });
        if replace {
            previous.insert(key, watermark);
        }
    }
    previous
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

fn action(outcome: ExecutionOutcome) -> ReconcileAction {
    ReconcileAction {
        requeue_after: outcome.requeue_after,
        kind: match outcome.kind {
            ExecutionKind::Stable => ReconcileKind::Stable,
            ExecutionKind::Applied => ReconcileKind::Applied,
            ExecutionKind::Executed => ReconcileKind::Executed,
            ExecutionKind::Waiting => ReconcileKind::Waiting,
            ExecutionKind::Unsafe => ReconcileKind::Unsafe,
        },
    }
}
