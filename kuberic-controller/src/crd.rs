use std::{collections::BTreeMap, net::SocketAddrV4};

use anyhow::{ensure, Result};
use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServiceTemplate {
    pub application: String,
    pub application_type: String,
    pub application_number: u32,
    pub package: String,
    pub service_type: String,
    pub name: String,
    pub package_version_instance: u64,
}

impl ServiceTemplate {
    pub fn fm(&self) -> kuberic_fm::ServiceConfig {
        kuberic_fm::ServiceConfig {
            application: self.application.clone(),
            application_type: self.application_type.clone(),
            application_number: self.application_number,
            package: self.package.clone(),
            service_type: self.service_type.clone(),
            name: self.name.clone(),
            package_version_instance: self.package_version_instance,
        }
    }
}

#[derive(CustomResource, Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[kube(
    group = "operator.kuberic.io",
    version = "v1alpha2",
    kind = "KubericSet",
    plural = "kubericsets",
    namespaced,
    status = "KubericSetStatus",
    derive = "PartialEq"
)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KubericSetSpec {
    #[schemars(range(min = 3, max = 3))]
    pub replicas: u32,
    pub image: String,
    pub package_path: String,
    pub client_port: u16,
    pub service: ServiceTemplate,
    #[serde(default)]
    pub parameters: BTreeMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FmAuthority {
    pub id: String,
    pub instance: u64,
    pub address: String,
    pub generation: i64,
}

impl FmAuthority {
    pub fn native(&self) -> Result<kuberic_fm::Fm> {
        Ok(kuberic_fm::Fm {
            identity: kuberic_fm::Identity {
                id: self.id.clone(),
                instance: self.instance,
            },
            address: self.address.parse::<SocketAddrV4>()?,
            generation: self.generation,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeAllocation {
    pub node_id: String,
    pub replica_id: i64,
    pub pod_name: String,
    pub config_map_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Allocation {
    pub resource_uid: String,
    pub resource_name: String,
    pub namespace: String,
    pub generation: i64,
    pub deployment: String,
    pub partition: String,
    pub fm: FmAuthority,
    pub spec: KubericSetSpec,
    pub nodes: Vec<NodeAllocation>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Binding {
    pub pod_uid: String,
    pub config_map_uid: String,
    pub ip: String,
    pub control_port: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FrozenCohort {
    pub allocation: Allocation,
    pub bindings: Vec<Binding>,
}

impl FrozenCohort {
    pub fn fm_config(&self) -> Result<kuberic_fm::Config> {
        ensure!(
            self.bindings.len() == 3 && self.allocation.nodes.len() == 3,
            "incomplete frozen cohort"
        );
        let allocation = &self.allocation;
        let config = kuberic_fm::Config {
            schema_version: 1,
            deployment: allocation.deployment.clone(),
            fm: allocation.fm.native()?,
            nodes: allocation
                .nodes
                .iter()
                .zip(&self.bindings)
                .map(|(node, binding)| {
                    Ok(kuberic_fm::Placement {
                        identity: kuberic_fm::Identity {
                            id: node.node_id.clone(),
                            instance: 1,
                        },
                        address: SocketAddrV4::new(binding.ip.parse()?, binding.control_port),
                        replica_id: node.replica_id,
                        replica_instance: 1,
                    })
                })
                .collect::<Result<Vec<_>>>()?,
            partition: allocation.partition.clone(),
            primary_node_id: allocation.nodes[1].node_id.clone(),
            service: allocation.spec.service.fm(),
            retry_interval_ms: 15_000,
            operation_timeout_ms: 120_000,
            shutdown_timeout_ms: 30_000,
        };
        config.validate()?;
        Ok(config)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ControllerPhase {
    #[default]
    Provisioning,
    Configuring,
    Starting,
    Serving,
    Stopping,
    Stopped,
    Blocked,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TeardownStage {
    NativeDeletion,
    Pods,
    Resources,
    Complete,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Teardown {
    pub cohort: FrozenCohort,
    pub service_uid: String,
    pub started_unix_ms: u64,
    pub deadline_unix_ms: u64,
    pub stage: TeardownStage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BootstrapWait {
    pub started_unix_ms: u64,
    pub deadline_unix_ms: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct KubericSetStatus {
    pub phase: ControllerPhase,
    pub allocation: Option<Allocation>,
    pub bindings: Option<Vec<Binding>>,
    pub service_endpoint: Option<String>,
    pub reason: Option<String>,
    #[serde(default)]
    pub bootstrap_wait: Option<BootstrapWait>,
    #[serde(default)]
    pub teardown: Option<Teardown>,
}

impl KubericSetStatus {
    pub fn cohort(&self) -> Option<FrozenCohort> {
        Some(FrozenCohort {
            allocation: self.allocation.clone()?,
            bindings: self.bindings.clone()?,
        })
    }
}
