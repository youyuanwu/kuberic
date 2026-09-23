use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::future::join_all;
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, PersistentVolumeClaim, PersistentVolumeClaimSpec, Pod, PodSpec,
    Service, ServicePort, ServiceSpec, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions};
use kube::{Api, Client, Resource, ResourceExt};
use kuberic_protocol::command::{EnsureConfiguration, InitializeAgentStore, ProtocolCommand};
use kuberic_protocol::types::{
    AcceptedStatus, EffectivePolicy, ReplicaId, ReplicaIdentity, ReplicaRole, ResourceUid,
    TransitionKind,
};
use kuberic_wire::proto;
use tokio::sync::Mutex;
use tonic::Code;

use crate::crd::{
    CONTROL_ADDRESS_ANNOTATION, CONTROLLER_NAME, INSTANCE_LABEL, KubericSet, KubericSetStatus,
    REPLICA_ID_LABEL, SET_UID_LABEL,
};
use crate::observation::{RawAgentObservation, RawObservation, RawObservationFailure};
use crate::{ControllerError, Result};

const CONTROL_PORT: i32 = 50051;
const REPLICATION_PORT: i32 = 50052;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectRecord {
    EnsureScaffolding(Vec<ReplicaId>),
    ReplaceStatus,
    RemoveWriteRouting,
    PublishWriteRouting(ReplicaIdentity),
    Execute(ProtocolCommand),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentRpcError {
    Unavailable(String),
    Invalid(String),
}

#[async_trait]
pub trait AgentApi: Send + Sync {
    async fn get_status(
        &self,
        endpoint: &str,
        token: &str,
        request: proto::GetAgentStatusRequest,
    ) -> std::result::Result<proto::AgentStatusReport, AgentRpcError>;

    async fn execute(
        &self,
        endpoint: &str,
        token: &str,
        request: proto::ExecuteCommandRequest,
    ) -> std::result::Result<proto::ExecuteCommandResponse, AgentRpcError>;
}

#[async_trait]
pub trait ClusterApi: Send + Sync {
    async fn observe(&self, namespace: &str, name: &str) -> Result<RawObservation>;

    async fn ensure_replica_scaffolding(
        &self,
        observation: &RawObservation,
        replica_ids: &[ReplicaId],
    ) -> Result<()>;

    async fn replace_status(
        &self,
        observation: &RawObservation,
        status: &AcceptedStatus,
    ) -> Result<()>;

    async fn remove_write_routing(&self, observation: &RawObservation) -> Result<()>;

    async fn publish_write_routing(
        &self,
        observation: &RawObservation,
        primary: &ReplicaIdentity,
    ) -> Result<()>;

    async fn execute_command(
        &self,
        observation: &RawObservation,
        command: &ProtocolCommand,
    ) -> Result<()>;
}

#[derive(Clone)]
pub struct GrpcAgentApi {
    deadline: Duration,
}

impl GrpcAgentApi {
    pub fn new(deadline: Duration) -> Self {
        Self { deadline }
    }
}

#[async_trait]
impl AgentApi for GrpcAgentApi {
    async fn get_status(
        &self,
        endpoint: &str,
        token: &str,
        request: proto::GetAgentStatusRequest,
    ) -> std::result::Result<proto::AgentStatusReport, AgentRpcError> {
        let mut client = tokio::time::timeout(
            self.deadline,
            proto::agent_control_client::AgentControlClient::connect(endpoint.to_string()),
        )
        .await
        .map_err(|_| AgentRpcError::Unavailable("agent connection timed out".to_string()))?
        .map_err(|error| AgentRpcError::Unavailable(error.to_string()))?;
        let mut request = tonic::Request::new(request);
        add_bearer_token(&mut request, token)?;
        tokio::time::timeout(self.deadline, client.get_status(request))
            .await
            .map_err(|_| AgentRpcError::Unavailable("GetStatus timed out".to_string()))?
            .map(|response| response.into_inner())
            .map_err(classify_status)
    }

    async fn execute(
        &self,
        endpoint: &str,
        token: &str,
        request: proto::ExecuteCommandRequest,
    ) -> std::result::Result<proto::ExecuteCommandResponse, AgentRpcError> {
        let mut client = tokio::time::timeout(
            self.deadline,
            proto::agent_control_client::AgentControlClient::connect(endpoint.to_string()),
        )
        .await
        .map_err(|_| AgentRpcError::Unavailable("agent connection timed out".to_string()))?
        .map_err(|error| AgentRpcError::Unavailable(error.to_string()))?;
        let mut request = tonic::Request::new(request);
        add_bearer_token(&mut request, token)?;
        tokio::time::timeout(self.deadline, client.execute(request))
            .await
            .map_err(|_| AgentRpcError::Unavailable("Execute timed out".to_string()))?
            .map(|response| response.into_inner())
            .map_err(classify_status)
    }
}

fn add_bearer_token<T>(
    request: &mut tonic::Request<T>,
    token: &str,
) -> std::result::Result<(), AgentRpcError> {
    let value = format!("Bearer {token}")
        .parse()
        .map_err(|error| AgentRpcError::Invalid(format!("invalid bearer token: {error}")))?;
    request.metadata_mut().insert("authorization", value);
    Ok(())
}

fn classify_status(status: tonic::Status) -> AgentRpcError {
    match status.code() {
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled => {
            AgentRpcError::Unavailable(status.to_string())
        }
        _ => AgentRpcError::Invalid(status.to_string()),
    }
}

pub struct KubeClusterApi<A> {
    client: Client,
    agents: Arc<A>,
    bearer_token: Arc<str>,
}

impl<A> KubeClusterApi<A> {
    pub fn new(client: Client, agents: Arc<A>, bearer_token: impl Into<Arc<str>>) -> Result<Self> {
        let bearer_token = bearer_token.into();
        if bearer_token.is_empty() {
            return Err(ControllerError::Effect(
                "agent bearer token must not be empty".to_string(),
            ));
        }
        Ok(Self {
            client,
            agents,
            bearer_token,
        })
    }
}

#[async_trait]
impl<A> ClusterApi for KubeClusterApi<A>
where
    A: AgentApi + 'static,
{
    async fn observe(&self, namespace: &str, name: &str) -> Result<RawObservation> {
        let sets: Api<KubericSet> = Api::namespaced(self.client.clone(), namespace);
        let set = sets
            .get(name)
            .await
            .map_err(|error| ControllerError::Observation(error.to_string()))?;
        let uid = set
            .uid()
            .ok_or_else(|| ControllerError::Observation("KubericSet has no UID".to_string()))?;
        let selector = format!("{SET_UID_LABEL}={uid}");
        let pods_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pvcs_api: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), namespace);
        let services_api: Api<Service> = Api::namespaced(self.client.clone(), namespace);
        let params = ListParams::default().labels(&selector);
        let (pods_result, pvcs_result, services_result) = tokio::join!(
            pods_api.list(&params),
            pvcs_api.list(&params),
            services_api.list(&params)
        );
        let mut failures = Vec::new();
        let pods = list_or_failure(pods_result, "pods", &mut failures);
        let pvcs = list_or_failure(pvcs_result, "pvcs", &mut failures);
        let services = list_or_failure(services_result, "services", &mut failures);
        let resource_uid = ResourceUid::new(uid);
        let agent_requests = pods
            .iter()
            .filter_map(|pod| {
                let replica_id = pod
                    .labels()
                    .get(REPLICA_ID_LABEL)?
                    .parse::<i64>()
                    .ok()
                    .filter(|value| *value > 0)
                    .map(ReplicaId::new)?;
                Some((replica_id, pod.clone()))
            })
            .map(|(replica_id, pod)| {
                let resource_uid = resource_uid.clone();
                async move {
                    (
                        replica_id,
                        self.observe_agent(&pod, &resource_uid, replica_id).await,
                    )
                }
            });
        let agents = join_all(agent_requests)
            .await
            .into_iter()
            .collect::<BTreeMap<_, _>>();

        Ok(RawObservation {
            set,
            pods,
            pvcs,
            services,
            agents,
            failures,
            now_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| ControllerError::Observation(error.to_string()))?
                .as_secs() as i64,
        })
    }

    async fn ensure_replica_scaffolding(
        &self,
        observation: &RawObservation,
        replica_ids: &[ReplicaId],
    ) -> Result<()> {
        let namespace = observation
            .set
            .namespace()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no namespace".to_string()))?;
        let uid = observation
            .set
            .uid()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no UID".to_string()))?;
        let owner = owner_reference(&observation.set)?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &namespace);
        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), &namespace);
        for replica_id in replica_ids {
            let pod_name = replica_name(&observation.set, *replica_id);
            let pvc_name = format!("{pod_name}-data");
            if !observation
                .pvcs
                .iter()
                .any(|pvc| pvc.name_any() == pvc_name)
            {
                create_exact(
                    &pvcs,
                    &pvc_name,
                    &replica_pvc(&observation.set, *replica_id, &uid, &owner),
                )
                .await?;
            }
            if !observation
                .pods
                .iter()
                .any(|pod| pod.name_any() == pod_name)
            {
                create_exact(
                    &pods,
                    &pod_name,
                    &replica_pod(&observation.set, *replica_id, &uid, &owner, &pvc_name),
                )
                .await?;
            }
        }
        ensure_write_service(self.client.clone(), observation, &namespace, &uid, &owner).await
    }

    async fn replace_status(
        &self,
        observation: &RawObservation,
        status: &AcceptedStatus,
    ) -> Result<()> {
        let namespace = observation
            .set
            .namespace()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no namespace".to_string()))?;
        let sets: Api<KubericSet> = Api::namespaced(self.client.clone(), &namespace);
        let mut set = observation.set.clone();
        set.status = Some(KubericSetStatus {
            authority: status.clone(),
        });
        sets.replace_status(&set.name_any(), &PostParams::default(), &set)
            .await
            .map(|_| ())
            .map_err(map_kube_status_error)
    }

    async fn remove_write_routing(&self, observation: &RawObservation) -> Result<()> {
        let service_name = format!("{}-write", observation.set.name_any());
        if !observation
            .services
            .iter()
            .any(|service| service.name_any() == service_name)
        {
            return Ok(());
        }
        patch_write_service(self.client.clone(), observation, "disabled").await
    }

    async fn publish_write_routing(
        &self,
        observation: &RawObservation,
        primary: &ReplicaIdentity,
    ) -> Result<()> {
        let namespace = observation
            .set
            .namespace()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no namespace".to_string()))?;
        let pod = observation
            .pods
            .iter()
            .find(|pod| pod.uid().as_deref() == Some(primary.instance_id.as_str()))
            .ok_or(ControllerError::ObservationStale)?;
        patch_pod_instance(
            self.client.clone(),
            &namespace,
            pod,
            primary.instance_id.as_str(),
        )
        .await?;
        patch_write_service(
            self.client.clone(),
            observation,
            primary.instance_id.as_str(),
        )
        .await
    }

    async fn execute_command(
        &self,
        observation: &RawObservation,
        command: &ProtocolCommand,
    ) -> Result<()> {
        let (target, replica_id) = command_target(command);
        let pod = observation
            .pods
            .iter()
            .find(|pod| {
                pod.labels()
                    .get(REPLICA_ID_LABEL)
                    .is_some_and(|value| value == &replica_id.to_string())
                    && pod.uid().as_deref() == Some(target.instance_id.as_str())
            })
            .ok_or(ControllerError::ObservationStale)?;
        let endpoint = agent_endpoint(pod).ok_or_else(|| {
            ControllerError::AgentUnavailable(format!(
                "replica {replica_id} has no control address"
            ))
        })?;
        let resource_uid = observation
            .set
            .uid()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no UID".to_string()))?;
        let response = self
            .agents
            .execute(
                &endpoint,
                &self.bearer_token,
                command_request(resource_uid, target, command.clone()),
            )
            .await
            .map_err(map_agent_effect_error)?;
        if response.protocol_version != kuberic_protocol::PROTOCOL_VERSION {
            return Err(ControllerError::InvalidAgentEvidence(
                "Execute response uses an unsupported protocol version".to_string(),
            ));
        }
        let report = response.observation.ok_or_else(|| {
            ControllerError::InvalidAgentEvidence(
                "Execute response omitted its resulting observation".to_string(),
            )
        })?;
        kuberic_wire::validate_agent_status_report(&report)
            .map_err(|error| ControllerError::InvalidAgentEvidence(error.to_string()))
    }
}

impl<A> KubeClusterApi<A>
where
    A: AgentApi,
{
    async fn observe_agent(
        &self,
        pod: &Pod,
        resource_uid: &ResourceUid,
        replica_id: ReplicaId,
    ) -> RawAgentObservation {
        let Some(instance_id) = pod.uid() else {
            return RawAgentObservation::Invalid {
                message: "Pod has no UID".to_string(),
            };
        };
        let Some(endpoint) = agent_endpoint(pod) else {
            return RawAgentObservation::Unavailable {
                message: "Pod has no control address while the agent starts".to_string(),
            };
        };
        let request = proto::GetAgentStatusRequest {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            resource_uid: resource_uid.to_string(),
            replica_id: replica_id.value(),
            expected_instance_id: instance_id,
        };
        match self
            .agents
            .get_status(&endpoint, &self.bearer_token, request)
            .await
        {
            Ok(report) => RawAgentObservation::Report(Box::new(report)),
            Err(AgentRpcError::Unavailable(message)) => {
                RawAgentObservation::Unavailable { message }
            }
            Err(AgentRpcError::Invalid(message)) => RawAgentObservation::Invalid { message },
        }
    }
}

fn list_or_failure<K>(
    result: std::result::Result<kube::api::ObjectList<K>, kube::Error>,
    source: &str,
    failures: &mut Vec<RawObservationFailure>,
) -> Vec<K>
where
    K: Clone,
{
    match result {
        Ok(list) => list.items,
        Err(error) => {
            failures.push(RawObservationFailure {
                source: source.to_string(),
                message: error.to_string(),
            });
            Vec::new()
        }
    }
}

async fn create_exact<K>(api: &Api<K>, name: &str, object: &K) -> Result<()>
where
    K: Clone
        + std::fmt::Debug
        + serde::Serialize
        + serde::de::DeserializeOwned
        + kube::Resource<DynamicType = ()>,
{
    api.create(&PostParams::default(), object)
        .await
        .map(|_| ())
        .map_err(|error| match error {
            kube::Error::Api(response) if response.code == 409 => ControllerError::ObservationStale,
            other => ControllerError::Effect(format!("creating {name}: {other}")),
        })
}

fn owner_reference(set: &KubericSet) -> Result<OwnerReference> {
    set.controller_owner_ref(&())
        .ok_or_else(|| ControllerError::Effect("KubericSet has no owner identity".to_string()))
}

fn base_labels(
    set: &KubericSet,
    replica_id: Option<ReplicaId>,
    uid: &str,
) -> BTreeMap<String, String> {
    let mut labels = BTreeMap::from([
        ("app.kubernetes.io/name".to_string(), "kuberic".to_string()),
        (
            "app.kubernetes.io/managed-by".to_string(),
            CONTROLLER_NAME.to_string(),
        ),
        (SET_UID_LABEL.to_string(), uid.to_string()),
        ("operator.kuberic.io/set-name".to_string(), set.name_any()),
    ]);
    if let Some(replica_id) = replica_id {
        labels.insert(REPLICA_ID_LABEL.to_string(), replica_id.to_string());
    }
    labels
}

fn replica_name(set: &KubericSet, replica_id: ReplicaId) -> String {
    format!("{}-{}", set.name_any(), replica_id.value())
}

fn replica_pvc(
    set: &KubericSet,
    replica_id: ReplicaId,
    uid: &str,
    owner: &OwnerReference,
) -> PersistentVolumeClaim {
    PersistentVolumeClaim {
        metadata: kube::core::ObjectMeta {
            name: Some(format!("{}-data", replica_name(set, replica_id))),
            labels: Some(base_labels(set, Some(replica_id), uid)),
            owner_references: Some(vec![owner.clone()]),
            ..Default::default()
        },
        spec: Some(PersistentVolumeClaimSpec {
            access_modes: Some(vec!["ReadWriteOnce".to_string()]),
            resources: Some(k8s_openapi::api::core::v1::VolumeResourceRequirements {
                requests: Some(BTreeMap::from([(
                    "storage".to_string(),
                    Quantity("1Gi".to_string()),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn replica_pod(
    set: &KubericSet,
    replica_id: ReplicaId,
    uid: &str,
    owner: &OwnerReference,
    pvc_name: &str,
) -> Pod {
    let labels = base_labels(set, Some(replica_id), uid);
    Pod {
        metadata: kube::core::ObjectMeta {
            name: Some(replica_name(set, replica_id)),
            labels: Some(labels),
            owner_references: Some(vec![owner.clone()]),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "application".to_string(),
                image: Some(set.spec.image.clone()),
                ports: Some(vec![
                    ContainerPort {
                        container_port: CONTROL_PORT,
                        name: Some("control".to_string()),
                        ..Default::default()
                    },
                    ContainerPort {
                        container_port: REPLICATION_PORT,
                        name: Some("replication".to_string()),
                        ..Default::default()
                    },
                ]),
                volume_mounts: Some(vec![VolumeMount {
                    mount_path: "/var/lib/kuberic".to_string(),
                    name: "data".to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            volumes: Some(vec![Volume {
                name: "data".to_string(),
                persistent_volume_claim: Some(
                    k8s_openapi::api::core::v1::PersistentVolumeClaimVolumeSource {
                        claim_name: pvc_name.to_string(),
                        ..Default::default()
                    },
                ),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    }
}

async fn ensure_write_service(
    client: Client,
    observation: &RawObservation,
    namespace: &str,
    uid: &str,
    owner: &OwnerReference,
) -> Result<()> {
    let service_name = format!("{}-write", observation.set.name_any());
    if observation
        .services
        .iter()
        .any(|service| service.name_any() == service_name)
    {
        return Ok(());
    }
    let services: Api<Service> = Api::namespaced(client, namespace);
    let service = Service {
        metadata: kube::core::ObjectMeta {
            name: Some(service_name.clone()),
            labels: Some(base_labels(&observation.set, None, uid)),
            owner_references: Some(vec![owner.clone()]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            selector: Some(BTreeMap::from([(
                INSTANCE_LABEL.to_string(),
                "disabled".to_string(),
            )])),
            ports: Some(vec![ServicePort {
                name: Some("application".to_string()),
                port: 80,
                target_port: Some(
                    k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(80),
                ),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    create_exact(&services, &service_name, &service).await
}

async fn patch_pod_instance(
    client: Client,
    namespace: &str,
    pod: &Pod,
    instance: &str,
) -> Result<()> {
    let uid = pod.uid().ok_or(ControllerError::ObservationStale)?;
    let resource_version = pod
        .resource_version()
        .ok_or(ControllerError::ObservationStale)?;
    let mut labels = pod.labels().clone();
    labels.insert(INSTANCE_LABEL.to_string(), instance.to_string());
    let patch = serde_json::json!([
        {"op": "test", "path": "/metadata/uid", "value": uid},
        {"op": "test", "path": "/metadata/resourceVersion", "value": resource_version},
        {"op": "add", "path": "/metadata/labels", "value": labels}
    ]);
    let pods: Api<Pod> = Api::namespaced(client, namespace);
    pods.patch(
        &pod.name_any(),
        &PatchParams::default(),
        &Patch::<serde_json::Value>::Json(
            serde_json::from_value(patch)
                .map_err(|error| ControllerError::Effect(error.to_string()))?,
        ),
    )
    .await
    .map(|_| ())
    .map_err(map_kube_effect_error)
}

async fn patch_write_service(
    client: Client,
    observation: &RawObservation,
    instance: &str,
) -> Result<()> {
    let namespace = observation
        .set
        .namespace()
        .ok_or_else(|| ControllerError::Effect("KubericSet has no namespace".to_string()))?;
    let service_name = format!("{}-write", observation.set.name_any());
    let service = observation
        .services
        .iter()
        .find(|service| service.name_any() == service_name)
        .ok_or(ControllerError::ObservationStale)?;
    let uid = service.uid().ok_or(ControllerError::ObservationStale)?;
    let resource_version = service
        .resource_version()
        .ok_or(ControllerError::ObservationStale)?;
    let patch = serde_json::json!([
        {"op": "test", "path": "/metadata/uid", "value": uid},
        {"op": "test", "path": "/metadata/resourceVersion", "value": resource_version},
        {"op": "add", "path": "/spec/selector", "value": {INSTANCE_LABEL: instance}}
    ]);
    let services: Api<Service> = Api::namespaced(client, &namespace);
    services
        .patch(
            &service_name,
            &PatchParams::default(),
            &Patch::<serde_json::Value>::Json(
                serde_json::from_value(patch)
                    .map_err(|error| ControllerError::Effect(error.to_string()))?,
            ),
        )
        .await
        .map(|_| ())
        .map_err(map_kube_effect_error)
}

fn agent_endpoint(pod: &Pod) -> Option<String> {
    pod.annotations()
        .get(CONTROL_ADDRESS_ANNOTATION)
        .cloned()
        .or_else(|| {
            pod.status
                .as_ref()
                .and_then(|status| status.pod_ip.as_ref())
                .map(|ip| format!("http://{ip}:{CONTROL_PORT}"))
        })
}

fn map_kube_effect_error(error: kube::Error) -> ControllerError {
    match error {
        kube::Error::Api(response) if response.code == 409 || response.code == 422 => {
            ControllerError::ObservationStale
        }
        other => ControllerError::Effect(other.to_string()),
    }
}

fn map_kube_status_error(error: kube::Error) -> ControllerError {
    match error {
        kube::Error::Api(response) if response.code == 409 => ControllerError::ObservationStale,
        other => ControllerError::Effect(other.to_string()),
    }
}

fn map_agent_effect_error(error: AgentRpcError) -> ControllerError {
    match error {
        AgentRpcError::Unavailable(message) => ControllerError::AgentUnavailable(message),
        AgentRpcError::Invalid(message) => ControllerError::InvalidAgentEvidence(message),
    }
}

fn command_target(command: &ProtocolCommand) -> (ReplicaIdentity, ReplicaId) {
    match command {
        ProtocolCommand::InitializeAgentStore(command) => {
            let identity = ReplicaIdentity {
                replica_id: command.local_replica_id,
                instance_id: command.expected_instance_id.clone(),
                agent_generation: command.assigned_agent_generation.clone(),
            };
            (identity, command.local_replica_id)
        }
        ProtocolCommand::EnsureConfiguration(command) => {
            let identity = ReplicaIdentity {
                replica_id: command.local_replica_id,
                instance_id: command.expected_instance_id.clone(),
                agent_generation: command.expected_agent_generation.clone(),
            };
            (identity, command.local_replica_id)
        }
    }
}

fn command_request(
    resource_uid: String,
    target: ReplicaIdentity,
    command: ProtocolCommand,
) -> proto::ExecuteCommandRequest {
    let command = match command {
        ProtocolCommand::InitializeAgentStore(command) => {
            proto::execute_command_request::Command::InitializeAgentStore(initialize_command(
                command,
            ))
        }
        ProtocolCommand::EnsureConfiguration(command) => {
            proto::execute_command_request::Command::EnsureConfiguration(ensure_command(*command))
        }
    };
    proto::ExecuteCommandRequest {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        resource_uid,
        target: Some(target.into()),
        command: Some(command),
    }
}

fn initialize_command(command: InitializeAgentStore) -> proto::InitializeAgentStoreCommand {
    proto::InitializeAgentStoreCommand {
        initialization_id: command.initialization_id.to_string(),
        resource_uid: command.resource_uid.to_string(),
        local_replica_id: command.local_replica_id.value(),
        expected_instance_id: command.expected_instance_id.to_string(),
        expected_pod_uid: command.expected_pod_uid.to_string(),
        expected_pvc_uid: command.expected_pvc_uid.to_string(),
        assigned_agent_generation: command.assigned_agent_generation.to_string(),
        effective_policy: Some(policy(command.effective_policy)),
    }
}

fn ensure_command(command: EnsureConfiguration) -> proto::EnsureConfigurationCommand {
    proto::EnsureConfigurationCommand {
        operation_id: command.operation_id.to_string(),
        previous_configuration: command.previous_configuration.map(Into::into),
        current_configuration: Some(command.current_configuration.into()),
        previous_epoch: command.previous_epoch.map(Into::into),
        current_epoch: Some(command.current_epoch.into()),
        effective_policy: Some(policy(command.effective_policy)),
        local_replica_id: command.local_replica_id.value(),
        expected_instance_id: command.expected_instance_id.to_string(),
        expected_agent_generation: command.expected_agent_generation.to_string(),
        transition_kind: transition_kind(command.transition_kind) as i32,
    }
}

fn policy(policy: EffectivePolicy) -> proto::EffectivePolicy {
    proto::EffectivePolicy {
        replica_set_size: policy.replica_set_size,
        write_quorum: policy.write_quorum,
        read_quorum: policy.read_quorum,
        failover_delay_seconds: policy.failover_delay_seconds,
    }
}

fn transition_kind(kind: TransitionKind) -> proto::TransitionKind {
    match kind {
        TransitionKind::Bootstrap => proto::TransitionKind::Bootstrap,
        TransitionKind::Replacement => proto::TransitionKind::Replacement,
        TransitionKind::Failover => proto::TransitionKind::Failover,
    }
}

#[derive(Clone)]
pub struct InMemoryClusterApi {
    state: Arc<Mutex<InMemoryState>>,
}

struct InMemoryState {
    observation: RawObservation,
    effects: Vec<EffectRecord>,
    conflict_next_status: bool,
    observation_count: usize,
    active_observations: usize,
    max_active_observations: usize,
    observation_delay: Duration,
}

impl InMemoryClusterApi {
    pub fn new(observation: RawObservation) -> Self {
        Self {
            state: Arc::new(Mutex::new(InMemoryState {
                observation,
                effects: Vec::new(),
                conflict_next_status: false,
                observation_count: 0,
                active_observations: 0,
                max_active_observations: 0,
                observation_delay: Duration::ZERO,
            })),
        }
    }

    pub async fn set_observation(&self, observation: RawObservation) {
        self.state.lock().await.observation = observation;
    }

    pub async fn conflict_next_status(&self) {
        self.state.lock().await.conflict_next_status = true;
    }

    pub async fn set_observation_delay(&self, delay: Duration) {
        self.state.lock().await.observation_delay = delay;
    }

    pub async fn effects(&self) -> Vec<EffectRecord> {
        self.state.lock().await.effects.clone()
    }

    pub async fn observation(&self) -> RawObservation {
        self.state.lock().await.observation.clone()
    }

    pub async fn observation_count(&self) -> usize {
        self.state.lock().await.observation_count
    }

    pub async fn max_active_observations(&self) -> usize {
        self.state.lock().await.max_active_observations
    }
}

#[async_trait]
impl ClusterApi for InMemoryClusterApi {
    async fn observe(&self, _namespace: &str, _name: &str) -> Result<RawObservation> {
        let delay = {
            let mut state = self.state.lock().await;
            state.observation_count += 1;
            state.active_observations += 1;
            state.max_active_observations =
                state.max_active_observations.max(state.active_observations);
            state.observation_delay
        };
        tokio::time::sleep(delay).await;
        let mut state = self.state.lock().await;
        state.active_observations -= 1;
        Ok(state.observation.clone())
    }

    async fn ensure_replica_scaffolding(
        &self,
        _observation: &RawObservation,
        replica_ids: &[ReplicaId],
    ) -> Result<()> {
        self.state
            .lock()
            .await
            .effects
            .push(EffectRecord::EnsureScaffolding(replica_ids.to_vec()));
        Ok(())
    }

    async fn replace_status(
        &self,
        observation: &RawObservation,
        status: &AcceptedStatus,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.conflict_next_status
            || state.observation.set.resource_version() != observation.set.resource_version()
        {
            state.conflict_next_status = false;
            return Err(ControllerError::ObservationStale);
        }
        state.observation.set.status = Some(KubericSetStatus {
            authority: status.clone(),
        });
        let next = state
            .observation
            .set
            .resource_version()
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_default()
            + 1;
        state.observation.set.metadata.resource_version = Some(next.to_string());
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
    }

    async fn remove_write_routing(&self, _observation: &RawObservation) -> Result<()> {
        self.state
            .lock()
            .await
            .effects
            .push(EffectRecord::RemoveWriteRouting);
        Ok(())
    }

    async fn publish_write_routing(
        &self,
        _observation: &RawObservation,
        primary: &ReplicaIdentity,
    ) -> Result<()> {
        self.state
            .lock()
            .await
            .effects
            .push(EffectRecord::PublishWriteRouting(primary.clone()));
        Ok(())
    }

    async fn execute_command(
        &self,
        _observation: &RawObservation,
        command: &ProtocolCommand,
    ) -> Result<()> {
        self.state
            .lock()
            .await
            .effects
            .push(EffectRecord::Execute(command.clone()));
        Ok(())
    }
}

#[allow(dead_code)]
async fn delete_exact<K>(api: &Api<K>, name: &str, uid: &str) -> Result<()>
where
    K: Clone + std::fmt::Debug + serde::de::DeserializeOwned + kube::Resource<DynamicType = ()>,
{
    api.delete(
        name,
        &DeleteParams {
            preconditions: Some(Preconditions {
                uid: Some(uid.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
    .map(|_| ())
    .map_err(map_kube_effect_error)
}

#[allow(dead_code)]
fn owned_selector(uid: &str) -> LabelSelector {
    LabelSelector {
        match_labels: Some(BTreeMap::from([(
            SET_UID_LABEL.to_string(),
            uid.to_string(),
        )])),
        ..Default::default()
    }
}

#[allow(dead_code)]
fn role_label(role: ReplicaRole) -> &'static str {
    match role {
        ReplicaRole::Primary => "primary",
        ReplicaRole::ActiveSecondary => "active-secondary",
        ReplicaRole::IdleSecondary => "idle-secondary",
        ReplicaRole::None => "none",
    }
}
