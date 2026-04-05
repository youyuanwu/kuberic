//! Custom Resource Definition for XdataApp
//!
//! This module defines the XdataApp custom resource that represents
//! a stateful xdata-app deployment managed by the operator.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// XdataApp is a custom resource that defines a stateful xdata-app deployment
#[derive(CustomResource, Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[kube(
    group = "xedio.io",
    version = "v1",
    kind = "XdataApp",
    plural = "xdataapps",
    shortname = "xapp",
    derive = "PartialEq",
    namespaced,
    status = "XdataAppStatus"
)]
#[serde(rename_all = "camelCase")]
pub struct XdataAppSpec {
    /// Number of replicas for the StatefulSet
    #[serde(default = "default_replicas")]
    pub replicas: i32,

    /// Container image to use
    #[serde(default = "default_image")]
    pub image: String,

    /// Image pull policy
    #[serde(default = "default_image_pull_policy")]
    pub image_pull_policy: String,

    /// Storage size for persistent volume claims
    #[serde(default = "default_storage")]
    pub storage: String,

    /// Resource requirements
    #[serde(default)]
    pub resources: ResourceRequirements,

    /// Port configuration
    #[serde(default = "default_port")]
    pub port: i32,

    /// Environment variables
    #[serde(default)]
    pub env: Vec<EnvVar>,

    /// Enable NodePort services for individual pods
    #[serde(default = "default_node_port_enabled")]
    pub node_port_enabled: bool,

    /// Base NodePort number (will use base, base+1, base+2, etc.)
    #[serde(default = "default_node_port_base")]
    pub node_port_base: i32,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ResourceRequirements {
    #[serde(default)]
    pub requests: Resources,
    #[serde(default)]
    pub limits: Resources,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct Resources {
    #[serde(default = "default_memory_request")]
    pub memory: String,
    #[serde(default = "default_cpu_request")]
    pub cpu: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema)]
pub struct EnvVar {
    pub name: String,
    pub value: String,
}

/// Status of the XdataApp
#[derive(Serialize, Deserialize, Debug, PartialEq, Clone, JsonSchema, Default)]
#[serde(rename_all = "camelCase")]
pub struct XdataAppStatus {
    /// Current state of the application
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,

    /// Number of ready replicas
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready_replicas: Option<i32>,

    /// Observed generation
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observed_generation: Option<i64>,

    /// Error message if any
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// Default implementations
fn default_replicas() -> i32 {
    3
}

fn default_image() -> String {
    "localhost/xdata-app:latest".to_string()
}

fn default_image_pull_policy() -> String {
    "IfNotPresent".to_string()
}

fn default_storage() -> String {
    "256Mi".to_string()
}

fn default_port() -> i32 {
    8080
}

fn default_memory_request() -> String {
    "64Mi".to_string()
}

fn default_cpu_request() -> String {
    "100m".to_string()
}

fn default_node_port_enabled() -> bool {
    true
}

fn default_node_port_base() -> i32 {
    30081
}

impl Default for ResourceRequirements {
    fn default() -> Self {
        Self {
            requests: Resources {
                memory: "64Mi".to_string(),
                cpu: "100m".to_string(),
            },
            limits: Resources {
                memory: "256Mi".to_string(),
                cpu: "500m".to_string(),
            },
        }
    }
}
