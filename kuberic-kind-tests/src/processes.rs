use std::collections::{BTreeMap, BTreeSet};

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use k8s_openapi::api::core::v1::Pod;
use kube::{api::PostParams, Api, Client, ResourceExt};
use kuberic_controller::{
    Allocation, BackendAdmission, CleanupState, FrozenCohort, NodeCleanup, ProcessApi,
    ProcessObservation,
};
use tokio::sync::Mutex;

use crate::node::Report;

const FINALIZER: &str = "testing.kuberic.io/cleanup-report";

pub(crate) struct PodProcesses {
    admission: BackendAdmission,
    pods: Api<Pod>,
    reports: Mutex<BTreeMap<String, Report>>,
    shutdown: Mutex<crate::shutdown::Shutdown>,
}

impl PodProcesses {
    pub async fn new(client: Client, namespace: &str, ip: &str) -> Result<Self> {
        Ok(Self {
            admission: BackendAdmission::default(),
            pods: Api::namespaced(client, namespace),
            reports: Mutex::new(BTreeMap::new()),
            shutdown: Mutex::new(crate::shutdown::Shutdown::bind(ip).await?),
        })
    }

    pub async fn verify_complete(&self, cohort: &FrozenCohort) -> Result<()> {
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
        println!(
            "{}",
            serde_json::json!({"event": "physical_cleanup", "reports": *reports})
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
        self.admission.require(&cohort.allocation).await?;
        let mut reports = self.reports.lock().await;
        let mut nodes = Vec::new();
        let mut deleting = BTreeSet::new();
        for (node, binding) in cohort.allocation.nodes.iter().zip(&cohort.bindings) {
            let pod = self.pods.get_opt(&node.pod_name).await?;
            let outcome = match pod {
                Some(mut pod) => {
                    ensure!(
                        pod.uid().as_deref() == Some(&binding.pod_uid),
                        "cleanup cannot adopt a replacement Pod"
                    );
                    if pod.metadata.deletion_timestamp.is_some() {
                        deleting.insert(binding.pod_uid.clone());
                    }
                    let terminated = pod
                        .status
                        .as_ref()
                        .and_then(|status| status.container_statuses.as_ref())
                        .and_then(|containers| {
                            containers.iter().find(|container| container.name == "node")
                        })
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
                        )?;
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
                        ensure!(
                            pod.metadata.deletion_timestamp.is_none(),
                            "Pod deletion began without a retained cleanup observer"
                        );
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
        self.shutdown
            .lock()
            .await
            .observe(cohort, &deleting)
            .await?;
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
        report.deployment == cohort.allocation.deployment
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
        let set: kuberic_controller::KubericSet = serde_json::from_str(include_str!(
            "../../kuberic-controller/examples/kubericset.json"
        ))
        .unwrap();
        let cohort = FrozenCohort {
            allocation: Allocation {
                resource_uid: "set-uid".into(),
                resource_name: "sample".into(),
                namespace: "test".into(),
                generation: 1,
                deployment: "deployment".into(),
                partition: "partition".into(),
                fm: kuberic_controller::FmAuthority {
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
            "deployment": "deployment", "pod_uid": "uid", "node_id": "node",
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
        for field in ["deployment", "pod_uid", "node_id"] {
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
