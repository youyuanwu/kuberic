use std::collections::BTreeMap;

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use k8s_openapi::api::core::v1::Pod;
use kube::{api::PostParams, Api, Client, ResourceExt};
use tokio::sync::Mutex;

use crate::{
    node::Report, resources, Allocation, BackendAdmission, CleanupState, FrozenCohort, NodeCleanup,
    ProcessApi, ProcessObservation,
};

pub const FINALIZER: &str = "operator.kuberic.io/cleanup-report";

pub struct PodProcesses {
    admission: BackendAdmission,
    pods: Api<Pod>,
    cohort: Mutex<Option<FrozenCohort>>,
    reports: Mutex<BTreeMap<String, Report>>,
}

impl PodProcesses {
    pub fn new(client: Client, namespace: &str) -> Self {
        Self {
            admission: BackendAdmission::default(),
            pods: Api::namespaced(client, namespace),
            cohort: Mutex::new(None),
            reports: Mutex::new(BTreeMap::new()),
        }
    }

    pub async fn reports(&self, cohort: &FrozenCohort) -> Result<BTreeMap<String, Report>> {
        self.admission.require(&cohort.allocation).await?;
        ensure!(
            self.cohort.lock().await.as_ref() == Some(cohort),
            "cleanup report cohort changed"
        );
        let reports = self.reports.lock().await;
        ensure!(
            reports.len() == cohort.bindings.len(),
            "missing original physical cleanup reports"
        );
        for binding in &cohort.bindings {
            reports
                .get(&binding.pod_uid)
                .context("missing Pod cleanup report")?
                .receipt()?;
        }
        Ok(reports.clone())
    }

    pub async fn verify_complete(&self, cohort: &FrozenCohort) -> Result<()> {
        let reports = self.reports(cohort).await?;
        println!(
            "{}",
            serde_json::json!({"event": "physical_cleanup", "reports": reports})
        );
        Ok(())
    }
}

#[async_trait]
impl ProcessApi for PodProcesses {
    async fn reserve(&self, allocation: &Allocation) -> Result<()> {
        self.admission.reserve(allocation).await
    }

    async fn allocation(&self) -> Result<Option<Allocation>> {
        Ok(self.admission.allocation().await)
    }

    async fn observe(&self, cohort: &FrozenCohort) -> Result<Option<ProcessObservation>> {
        if self.admission.allocation().await.as_ref() != Some(&cohort.allocation) {
            return Ok(None);
        }
        let mut original = self.cohort.lock().await;
        if let Some(original) = original.as_ref() {
            if original != cohort {
                return Ok(Some(ProcessObservation {
                    cohort: original.clone(),
                    nodes: original
                        .bindings
                        .iter()
                        .map(|binding| NodeCleanup {
                            pod_uid: binding.pod_uid.clone(),
                            outcome: CleanupState::Failed("original cleanup cohort changed".into()),
                        })
                        .collect(),
                }));
            }
        } else {
            ensure!(
                cohort.bindings.len() == 3 && cohort.allocation.nodes.len() == 3,
                "cannot observe an incomplete original cohort"
            );
            *original = Some(cohort.clone());
        }
        drop(original);
        let mut reports = self.reports.lock().await;
        let mut nodes = Vec::new();
        for (node, binding) in cohort.allocation.nodes.iter().zip(&cohort.bindings) {
            let pod = self.pods.get_opt(&node.pod_name).await?;
            let outcome = match pod {
                Some(mut pod) => {
                    let owned = pod
                        .metadata
                        .owner_references
                        .as_ref()
                        .is_some_and(|owners| {
                            owners
                                .iter()
                                .filter(|owner| owner.controller == Some(true))
                                .count()
                                == 1
                                && owners.iter().any(|owner| {
                                    owner.controller == Some(true)
                                        && owner.uid == cohort.allocation.resource_uid
                                        && owner.name == cohort.allocation.resource_name
                                        && owner.kind == "KubericSet"
                                        && owner.api_version == "operator.kuberic.io/v1alpha2"
                                })
                        });
                    if pod.uid().as_deref() != Some(&binding.pod_uid)
                        || !owned
                        || pod.labels().get(resources::SET_UID)
                            != Some(&cohort.allocation.resource_uid)
                        || pod.labels().get(resources::DEPLOYMENT)
                            != Some(&cohort.allocation.deployment)
                    {
                        nodes.push(NodeCleanup {
                            pod_uid: binding.pod_uid.clone(),
                            outcome: CleanupState::Failed(
                                "cleanup cannot adopt a replacement or foreign Pod".into(),
                            ),
                        });
                        continue;
                    }
                    let container = pod
                        .status
                        .as_ref()
                        .and_then(|status| status.container_statuses.as_ref())
                        .and_then(|containers| {
                            containers.iter().find(|container| container.name == "node")
                        });
                    if container.is_some_and(|container| container.restart_count != 0) {
                        nodes.push(NodeCleanup {
                            pod_uid: binding.pod_uid.clone(),
                            outcome: CleanupState::Failed(
                                "original node container restarted".into(),
                            ),
                        });
                        continue;
                    }
                    let terminated = container
                        .and_then(|container| container.state.as_ref())
                        .and_then(|state| state.terminated.as_ref());
                    if let Some(terminated) = terminated {
                        let report = validate_report(
                            cohort,
                            &binding.pod_uid,
                            &node.node_id,
                            terminated.exit_code,
                            terminated.signal,
                            terminated.message.as_deref(),
                        );
                        let report = match report {
                            Ok(report) => report,
                            Err(error) => {
                                nodes.push(NodeCleanup {
                                    pod_uid: binding.pod_uid.clone(),
                                    outcome: CleanupState::Failed(format!("{error:#}")),
                                });
                                continue;
                            }
                        };
                        reports.insert(binding.pod_uid.clone(), report);
                    }
                    let complete = reports.get(&binding.pod_uid);
                    let held = pod.finalizers().iter().any(|value| value == FINALIZER);
                    if complete.is_some() && held {
                        pod.metadata
                            .finalizers
                            .get_or_insert_default()
                            .retain(|value| value != FINALIZER);
                        self.pods
                            .replace(&node.pod_name, &PostParams::default(), &pod)
                            .await?;
                    } else if complete.is_none() && !held {
                        if pod.metadata.deletion_timestamp.is_some() {
                            nodes.push(NodeCleanup {
                                pod_uid: binding.pod_uid.clone(),
                                outcome: CleanupState::Failed(
                                    "Pod deletion began without a retained cleanup observer".into(),
                                ),
                            });
                            continue;
                        }
                        // Retain kubelet's actual termination message, not an annotation
                        // inferred from Pod disappearance or native FM completion.
                        pod.metadata
                            .finalizers
                            .get_or_insert_default()
                            .push(FINALIZER.into());
                        self.pods
                            .replace(&node.pod_name, &PostParams::default(), &pod)
                            .await?;
                    }
                    match complete {
                        Some(report) => CleanupState::Complete(report.receipt()?),
                        None => CleanupState::Pending,
                    }
                }
                None => match reports.get(&binding.pod_uid) {
                    Some(report) => CleanupState::Complete(report.receipt()?),
                    None => CleanupState::Failed(
                        "original Pod disappeared without a physical cleanup report".into(),
                    ),
                },
            };
            nodes.push(NodeCleanup {
                pod_uid: binding.pod_uid.clone(),
                outcome,
            });
        }
        Ok(Some(ProcessObservation {
            cohort: cohort.clone(),
            nodes,
        }))
    }
}

fn validate_report(
    cohort: &FrozenCohort,
    uid: &str,
    node_id: &str,
    exit_code: i32,
    signal: Option<i32>,
    message: Option<&str>,
) -> Result<Report> {
    ensure!(
        exit_code == 0 && signal.is_none_or(|value| value == 0),
        "original node container failed or was killed: exit={exit_code}, signal={signal:?}, message={message:?}"
    );
    let report: Report =
        serde_json::from_str(message.context("node exited without a cleanup report")?)?;
    ensure!(
        report.resource_uid == cohort.allocation.resource_uid
            && report.deployment == cohort.allocation.deployment
            && report.pod_uid == uid
            && report.node_id == node_id,
        "cleanup report changed original deployment, Pod UID or native identity"
    );
    report.receipt()?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_requires_successful_exact_nonforced_physical_evidence() {
        let set: crate::KubericSet =
            serde_json::from_str(include_str!("../examples/kubericset.json")).unwrap();
        let cohort = FrozenCohort {
            allocation: Allocation {
                resource_uid: "set-uid".into(),
                resource_name: "sample".into(),
                namespace: "test".into(),
                generation: 1,
                deployment: "deployment".into(),
                partition: "partition".into(),
                fm: crate::FmAuthority {
                    id: "fm".into(),
                    instance: 1,
                    address: "10.0.0.1:19000".into(),
                    generation: 1,
                },
                spec: set.spec,
                nodes: vec![],
            },
            bindings: vec![],
        };
        let report = serde_json::json!({
            "resource_uid": "set-uid", "deployment": "deployment", "pod_uid": "uid", "node_id": "node",
            "node_reaped": true, "node_exit_success": true,
            "application_reaped": true, "resources_joined": true, "forced": false
        });
        let validate = |value: &serde_json::Value, exit, signal| {
            validate_report(
                &cohort,
                "uid",
                "node",
                exit,
                signal,
                Some(&value.to_string()),
            )
        };
        assert!(validate(&report, 0, None).is_ok());
        assert!(validate(&report, 137, Some(9)).is_err());
        assert!(validate_report(&cohort, "uid", "node", 0, None, None).is_err());
        for field in ["resource_uid", "deployment", "pod_uid", "node_id"] {
            let mut changed = report.clone();
            changed[field] = "replacement".into();
            assert!(validate(&changed, 0, None).is_err());
        }
        for field in [
            "node_reaped",
            "node_exit_success",
            "application_reaped",
            "resources_joined",
            "forced",
        ] {
            let mut changed = report.clone();
            changed[field] = (field == "forced").into();
            assert!(validate(&changed, 0, None).is_err());
        }
    }
}
