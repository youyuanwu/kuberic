use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use futures::future::join_all;
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, EnvVarSource, ObjectFieldSelector, PersistentVolumeClaim,
    PersistentVolumeClaimSpec, Pod, PodSecurityContext, PodSpec, Secret, SecretKeySelector,
    Service, ServicePort, ServiceSpec, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, OwnerReference};
use kube::api::{DeleteParams, ListParams, Patch, PatchParams, PostParams, Preconditions};
use kube::{Api, Client, Resource, ResourceExt};
use kuberic_runtime::control::proto;
use kuberic_runtime::protocol::command::{InitializeAgentStore, ProtocolCommand};
use kuberic_runtime::protocol::observation::ReplicaObservationKey;
use kuberic_runtime::protocol::public_operations::{
    CanonicalReplicaRebuildRequired, PublicBuildAttemptReference, PublicInboundBuildAuthority,
    PublicOperationProgram, PublicV1AcceptedStatus, PublicV1AgentReport,
    PublicV1OperationCompletion, PublicV1OperationResult, PublicV1SessionRenewalStatus,
};
use kuberic_runtime::protocol::types::{
    ConfigurationDescriptor, EffectivePolicy, OperationId, PodUid, ProvisioningIntent, PvcUid,
    ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ResourceUid,
    derive_agent_generation, derive_initialization_id, derive_replacement_resource_name,
    derive_replica_endpoint_name,
};
#[cfg(feature = "runtime-test-bridge")]
use tokio::sync::Mutex;
use tonic::Code;

use crate::crd::{
    CONTROL_ADDRESS_ANNOTATION, CONTROLLER_NAME, INSTANCE_LABEL, KubericSet, KubericSetStatus,
    PublicV1AuthorityStatus, PublicV1OperationStatus, PublicV1SessionRenewalState,
    PublicV1TopologyState, PublicV1TransitionState, REPLICA_ID_LABEL,
    SCALE_UP_ALLOCATION_ANNOTATION, SET_UID_LABEL,
};
use crate::evaluator::public_v1_evidence::{
    PublicV1AcceptedTransitionStatus, PublicV1ResourceAuthority,
};
use crate::observation::{RawAgentObservation, RawObservation, RawObservationFailure};
use crate::{ControllerError, Result};

const CONTROL_PORT: i32 = 50051;
const REPLICATION_PORT: i32 = 50052;
const LIVE_TEST_COPY_GATE_ANNOTATION: &str = "testing.kuberic.io/live-copy-gate";
const LIVE_TEST_COPY_GATE_ADDRESS: &str = "0.0.0.0:18080";
#[cfg(feature = "runtime-test-bridge")]
pub(crate) const PUBLIC_V1_SERVICE_LOCATION_ANNOTATION: &str =
    "operator.kuberic.io/public-v1-service-location";
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EffectRecord {
    EnsureReplicaSupport,
    EnsureScaffolding(Vec<ReplicaId>),
    EnsureReplacement(ReplicaIdentity),
    DeleteScaffolding {
        pod_name: Option<String>,
        pvc_name: Option<String>,
    },
    ReplaceStatus,
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

    async fn ensure_replica_support(&self, observation: &RawObservation) -> Result<()>;

    async fn converge_public_v1_write_service(
        &self,
        observation: &RawObservation,
        primary: Option<&ReplicaIdentity>,
    ) -> Result<()>;

    async fn ensure_replacement_scaffolding(
        &self,
        observation: &RawObservation,
        replica_id: ReplicaId,
        replacing: &ReplicaIdentity,
    ) -> Result<()>;

    async fn delete_replica_scaffolding(
        &self,
        observation: &RawObservation,
        pod_name: Option<&str>,
        pod_uid: Option<&PodUid>,
        pvc_name: Option<&str>,
        pvc_uid: Option<&PvcUid>,
    ) -> Result<()>;

    async fn execute_command(
        &self,
        observation: &RawObservation,
        command: &ProtocolCommand,
    ) -> Result<()>;
}

pub trait ProductionClusterApi:
    ClusterApi
    + PublicV1OperationApi
    + PublicV1HistoryApi
    + PublicV1SessionRenewalApi
    + PublicV1EvidenceApi
{
}

impl<T> ProductionClusterApi for T where
    T: ClusterApi
        + PublicV1OperationApi
        + PublicV1HistoryApi
        + PublicV1SessionRenewalApi
        + PublicV1EvidenceApi
{
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
            .map_err(classify_execute_status)
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
        Code::Unavailable | Code::DeadlineExceeded | Code::Cancelled | Code::Unknown => {
            AgentRpcError::Unavailable(status.to_string())
        }
        _ => AgentRpcError::Invalid(status.to_string()),
    }
}

fn classify_execute_status(status: tonic::Status) -> AgentRpcError {
    // Session or authority may have advanced after GetStatus. A rejected
    // dispatch is not a contradictory report and must be resolved by observing.
    if matches!(
        status.code(),
        Code::FailedPrecondition
            | Code::Aborted
            | Code::Internal
            | Code::Unknown
            | Code::ResourceExhausted
    ) {
        AgentRpcError::Unavailable(status.to_string())
    } else {
        classify_status(status)
    }
}

pub struct KubeClusterApi<A> {
    client: Client,
    agents: Arc<A>,
    bearer_token: Arc<str>,
    installation_namespace: Arc<str>,
}

impl<A> KubeClusterApi<A> {
    pub fn new(
        client: Client,
        agents: Arc<A>,
        bearer_token: impl Into<Arc<str>>,
        installation_namespace: impl Into<Arc<str>>,
    ) -> Result<Self> {
        let bearer_token = bearer_token.into();
        if bearer_token.is_empty() {
            return Err(ControllerError::Effect(
                "agent bearer token must not be empty".to_string(),
            ));
        }
        let installation_namespace = installation_namespace.into();
        if !Self::valid_namespace(&installation_namespace) {
            return Err(ControllerError::Effect(
                "installation namespace must be a valid DNS label".to_string(),
            ));
        }
        Ok(Self {
            client,
            agents,
            bearer_token,
            installation_namespace,
        })
    }

    fn require_installation_namespace(&self, namespace: &str) -> Result<()> {
        Self::namespace_mismatch(&self.installation_namespace, namespace)
    }

    fn namespace_mismatch(configured: &str, requested: &str) -> Result<()> {
        if configured == requested {
            Ok(())
        } else {
            Err(ControllerError::Observation(
                "requested namespace differs from the controller installation namespace"
                    .to_string(),
            ))
        }
    }

    fn valid_namespace(namespace: &str) -> bool {
        !namespace.is_empty()
            && namespace.len() <= 63
            && namespace
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && namespace
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric)
            && namespace
                .as_bytes()
                .last()
                .is_some_and(u8::is_ascii_alphanumeric)
    }
}

#[async_trait]
impl<A> ClusterApi for KubeClusterApi<A>
where
    A: AgentApi + 'static,
{
    async fn observe(&self, namespace: &str, name: &str) -> Result<RawObservation> {
        self.require_installation_namespace(namespace)?;
        let sets: Api<KubericSet> = Api::namespaced(self.client.clone(), namespace);
        let set = sets.get(name).await.map_err(map_kube_observation_error)?;
        let uid = set
            .uid()
            .ok_or_else(|| ControllerError::Observation("KubericSet has no UID".to_string()))?;
        let selector = format!("{SET_UID_LABEL}={uid}");
        let pods_api: Api<Pod> = Api::namespaced(self.client.clone(), namespace);
        let pvcs_api: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), namespace);
        let services_api: Api<Service> = Api::namespaced(self.client.clone(), namespace);
        let secrets_api: Api<Secret> = Api::namespaced(self.client.clone(), namespace);
        let params = ListParams::default().labels(&selector);
        let (pods_result, pvcs_result, services_result, secrets_result) = tokio::join!(
            pods_api.list(&params),
            pvcs_api.list(&params),
            services_api.list(&params),
            secrets_api.list(&params)
        );
        let mut failures = Vec::new();
        let pods = list_or_failure(pods_result, "pods", &mut failures);
        let pvcs = list_or_failure(pvcs_result, "pvcs", &mut failures);
        let services = list_or_failure(services_result, "services", &mut failures);
        let secrets = list_or_failure(secrets_result, "secrets", &mut failures);
        let resource_uid = ResourceUid::new(uid);
        let mut raw = RawObservation {
            set,
            pods,
            pvcs,
            services,
            secrets,
            failures,
            agents: BTreeMap::new(),
            now_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| ControllerError::Observation(error.to_string()))?
                .as_secs() as i64,
        };
        let agent_requests = raw
            .pods
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
                    let instance_id = pod.uid().map(ReplicaInstanceId::new).unwrap_or_else(|| {
                        ReplicaInstanceId::new(format!("missing-pod-uid-{}", pod.name_any()))
                    });
                    (
                        ReplicaObservationKey::new(replica_id, instance_id),
                        self.observe_agent(&pod, &resource_uid, replica_id).await,
                    )
                }
            });
        raw.agents = join_all(agent_requests)
            .await
            .into_iter()
            .collect::<BTreeMap<_, _>>();
        Ok(raw)
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
        ensure_agent_credentials(
            self.client.clone(),
            observation,
            &namespace,
            &uid,
            &owner,
            &self.bearer_token,
        )
        .await?;
        ensure_peer_service(self.client.clone(), observation, &namespace, &uid, &owner).await?;
        let image = effective_replica_image(observation)?;
        for replica_id in replica_ids {
            let pod_name = replica_name(&observation.set, *replica_id);
            let pvc_name = format!("{pod_name}-data");
            let pvc = observation
                .pvcs
                .iter()
                .find(|pvc| pvc.name_any() == pvc_name);
            let Some(pvc) = pvc else {
                create_exact(
                    &pvcs,
                    &pvc_name,
                    &replica_pvc(&observation.set, *replica_id, &uid, &owner),
                )
                .await?;
                continue;
            };
            let pvc_uid = pvc.uid().ok_or(ControllerError::ObservationStale)?;
            if !observation
                .pods
                .iter()
                .any(|pod| pod.name_any() == pod_name)
            {
                create_exact(
                    &pods,
                    &pod_name,
                    &replica_pod(
                        &observation.set,
                        *replica_id,
                        &uid,
                        &owner,
                        &pvc_name,
                        &pvc_uid,
                        &image,
                    ),
                )
                .await?;
                continue;
            }
            let pod = observation
                .pods
                .iter()
                .find(|pod| pod.name_any() == pod_name)
                .expect("observed existing replica Pod");
            ensure_exact_peer_endpoint(
                self.client.clone(),
                observation,
                &namespace,
                &uid,
                &owner,
                pod,
                pvc,
                *replica_id,
            )
            .await?;
        }
        ensure_write_service(self.client.clone(), observation, &namespace, &uid, &owner).await
    }

    async fn ensure_replacement_scaffolding(
        &self,
        observation: &RawObservation,
        replica_id: ReplicaId,
        replacing: &ReplicaIdentity,
    ) -> Result<()> {
        let namespace = observation
            .set
            .namespace()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no namespace".to_string()))?;
        let uid = observation
            .set
            .uid()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no UID".to_string()))?;
        let resource_uid = ResourceUid::new(&uid);
        let owner = owner_reference(&observation.set)?;
        let image = effective_replica_image(observation)?;
        let base = derive_replacement_resource_name(&resource_uid, replacing);
        let pod_name = format!("{}-{base}", observation.set.name_any());
        let pvc_name = format!("{pod_name}-data");
        let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), &namespace);
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &namespace);
        let pvc = observation
            .pvcs
            .iter()
            .find(|pvc| pvc.name_any() == pvc_name);
        let Some(pvc) = pvc else {
            create_exact(
                &pvcs,
                &pvc_name,
                &replica_pvc_named(&observation.set, replica_id, &uid, &owner, &pvc_name),
            )
            .await?;
            return Ok(());
        };
        let pvc_uid = pvc.uid().ok_or(ControllerError::ObservationStale)?;
        let pod = observation
            .pods
            .iter()
            .find(|pod| pod.name_any() == pod_name);
        let Some(pod) = pod else {
            create_exact(
                &pods,
                &pod_name,
                &replica_pod_named(
                    &observation.set,
                    replica_id,
                    &uid,
                    &owner,
                    &pod_name,
                    &pvc_name,
                    &pvc_uid,
                    &image,
                    None,
                ),
            )
            .await?;
            return Ok(());
        };
        ensure_exact_peer_endpoint(
            self.client.clone(),
            observation,
            &namespace,
            &uid,
            &owner,
            pod,
            pvc,
            replica_id,
        )
        .await
    }

    async fn delete_replica_scaffolding(
        &self,
        observation: &RawObservation,
        pod_name: Option<&str>,
        pod_uid: Option<&PodUid>,
        pvc_name: Option<&str>,
        pvc_uid: Option<&PvcUid>,
    ) -> Result<()> {
        let namespace = observation
            .set
            .namespace()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no namespace".to_string()))?;
        if let Some(pod_uid) = pod_uid
            && let Some(service) = observation.services.iter().find(|service| {
                service.spec.as_ref().is_some_and(|spec| {
                    spec.selector.as_ref().is_some_and(|selector| {
                        selector.get(INSTANCE_LABEL).map(String::as_str) == Some(pod_uid.as_str())
                    })
                })
            })
        {
            let services: Api<Service> = Api::namespaced(self.client.clone(), &namespace);
            let service_uid = service.uid().ok_or(ControllerError::ObservationStale)?;
            delete_exact(&services, &service.name_any(), &service_uid).await?;
        }
        let pod = pod_uid.and_then(|uid| {
            observation
                .pods
                .iter()
                .find(|pod| pod.uid().as_deref() == Some(uid.as_str()))
        });
        let pod_name = pod_name
            .map(str::to_string)
            .or_else(|| pod.map(ResourceExt::name_any));
        if let (Some(name), Some(uid)) = (pod_name.as_deref(), pod_uid) {
            let pods: Api<Pod> = Api::namespaced(self.client.clone(), &namespace);
            delete_exact(&pods, name, uid.as_str()).await?;
        }
        let pvc = pvc_uid.and_then(|uid| {
            observation
                .pvcs
                .iter()
                .find(|pvc| pvc.uid().as_deref() == Some(uid.as_str()))
        });
        let pvc_name = pvc_name
            .map(str::to_string)
            .or_else(|| pvc.map(ResourceExt::name_any));
        if let (Some(name), Some(uid)) = (pvc_name.as_deref(), pvc_uid) {
            let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(self.client.clone(), &namespace);
            delete_exact(&pvcs, name, uid.as_str()).await?;
        }
        Ok(())
    }

    async fn ensure_replica_support(&self, observation: &RawObservation) -> Result<()> {
        let namespace = observation
            .set
            .namespace()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no namespace".to_string()))?;
        let uid = observation
            .set
            .uid()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no UID".to_string()))?;
        let owner = owner_reference(&observation.set)?;
        ensure_agent_credentials(
            self.client.clone(),
            observation,
            &namespace,
            &uid,
            &owner,
            &self.bearer_token,
        )
        .await?;
        ensure_peer_service(self.client.clone(), observation, &namespace, &uid, &owner).await
    }

    async fn converge_public_v1_write_service(
        &self,
        observation: &RawObservation,
        primary: Option<&ReplicaIdentity>,
    ) -> Result<()> {
        let namespace = observation
            .set
            .namespace()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no namespace".to_string()))?;
        let selector = match primary {
            Some(primary) => {
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
                primary.instance_id.to_string()
            }
            None => "disabled".to_string(),
        };
        patch_public_v1_write_service(self.client.clone(), observation, &selector).await
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
        let expected_process_session_id =
            observed_process_session(observation, replica_id, &target)?;
        let response = self
            .agents
            .execute(
                &endpoint,
                &self.bearer_token,
                command_request(
                    resource_uid,
                    target,
                    expected_process_session_id,
                    command.clone(),
                )?,
            )
            .await
            .map_err(map_agent_effect_error)?;
        if response.protocol_version != kuberic_runtime::protocol::PROTOCOL_VERSION {
            return Err(ControllerError::InvalidAgentEvidence(
                "Execute response uses an unsupported protocol version".to_string(),
            ));
        }
        let report = response.observation.ok_or_else(|| {
            ControllerError::InvalidAgentEvidence(
                "Execute response omitted its resulting observation".to_string(),
            )
        })?;
        kuberic_runtime::control::normalize_public_v1_agent_status_report(report)
            .map(|_| ())
            .map_err(|error| ControllerError::InvalidAgentEvidence(error.to_string()))
    }
}

fn observed_process_session(
    observation: &RawObservation,
    replica_id: ReplicaId,
    target: &ReplicaIdentity,
) -> Result<String> {
    let observation_key = ReplicaObservationKey::new(replica_id, target.instance_id.clone());
    match observation.agents.get(&observation_key) {
        Some(RawAgentObservation::PublicV1Report(report))
            if !report.process_session_id.is_empty() =>
        {
            Ok(report.process_session_id.to_string())
        }
        Some(RawAgentObservation::Uninitialized(report))
            if !report.process_session_id.is_empty() =>
        {
            Ok(report.process_session_id.to_string())
        }

        _ => Err(ControllerError::ObservationStale),
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
            protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
            resource_uid: resource_uid.to_string(),
            replica_id: replica_id.value(),
            expected_instance_id: instance_id.clone(),
        };
        match self
            .agents
            .get_status(&endpoint, &self.bearer_token, request)
            .await
        {
            Ok(report) if report.public_v1.is_none() => {
                match kuberic_runtime::control::normalize_uninitialized_agent_status_report(report)
                {
                    Ok(report)
                        if report.resource_uid == *resource_uid
                            && report.replica_id == replica_id
                            && report.pod_uid.as_str() == instance_id =>
                    {
                        RawAgentObservation::Uninitialized(report)
                    }
                    Ok(_) => RawAgentObservation::Invalid {
                        message: "fresh agent status differs from observed resource identity"
                            .to_string(),
                    },
                    Err(error) => RawAgentObservation::Invalid {
                        message: error.to_string(),
                    },
                }
            }
            Ok(report) => {
                match kuberic_runtime::control::normalize_public_v1_agent_status_report(report) {
                    Ok(report) => RawAgentObservation::PublicV1Report(Box::new(report)),
                    Err(error) => RawAgentObservation::Invalid {
                        message: error.to_string(),
                    },
                }
            }
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
    replica_pvc_named(
        set,
        replica_id,
        uid,
        owner,
        &format!("{}-data", replica_name(set, replica_id)),
    )
}

fn replica_pvc_named(
    set: &KubericSet,
    replica_id: ReplicaId,
    uid: &str,
    owner: &OwnerReference,
    name: &str,
) -> PersistentVolumeClaim {
    PersistentVolumeClaim {
        metadata: kube::core::ObjectMeta {
            name: Some(name.to_string()),
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

#[allow(clippy::too_many_arguments)]
fn replica_pod(
    set: &KubericSet,
    replica_id: ReplicaId,
    uid: &str,
    owner: &OwnerReference,
    pvc_name: &str,
    pvc_uid: &str,
    image: &str,
) -> Pod {
    replica_pod_named(
        set,
        replica_id,
        uid,
        owner,
        &replica_name(set, replica_id),
        pvc_name,
        pvc_uid,
        image,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn replica_pod_named(
    set: &KubericSet,
    replica_id: ReplicaId,
    uid: &str,
    owner: &OwnerReference,
    pod_name: &str,
    pvc_name: &str,
    pvc_uid: &str,
    image: &str,
    allocation_operation_id: Option<&OperationId>,
) -> Pod {
    let labels = base_labels(set, Some(replica_id), uid);
    let annotations = allocation_operation_id.map(|operation_id| {
        BTreeMap::from([(
            SCALE_UP_ALLOCATION_ANNOTATION.to_string(),
            operation_id.to_string(),
        )])
    });
    let credential_name = agent_credential_name(set);
    let mut env = vec![
        EnvVar {
            name: "KUBERIC_RESOURCE_UID".to_string(),
            value: Some(uid.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "KUBERIC_REPLICA_ID".to_string(),
            value: Some(replica_id.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "KUBERIC_PVC_UID".to_string(),
            value: Some(pvc_uid.to_string()),
            ..Default::default()
        },
        EnvVar {
            name: "KUBERIC_SET_NAME".to_string(),
            value: Some(set.name_any()),
            ..Default::default()
        },
        EnvVar {
            name: "KUBERIC_NAMESPACE".to_string(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    api_version: Some("v1".to_string()),
                    field_path: "metadata.namespace".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        EnvVar {
            name: "KUBERIC_POD_UID".to_string(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    api_version: Some("v1".to_string()),
                    field_path: "metadata.uid".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        EnvVar {
            name: "KUBERIC_POD_IP".to_string(),
            value_from: Some(EnvVarSource {
                field_ref: Some(ObjectFieldSelector {
                    api_version: Some("v1".to_string()),
                    field_path: "status.podIP".to_string(),
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        EnvVar {
            name: "KUBERIC_AGENT_BEARER_TOKEN".to_string(),
            value_from: Some(EnvVarSource {
                secret_key_ref: Some(SecretKeySelector {
                    key: "bearer-token".to_string(),
                    name: credential_name,
                    optional: Some(false),
                }),
                ..Default::default()
            }),
            ..Default::default()
        },
        EnvVar {
            name: "KUBERIC_DATA_ROOT".to_string(),
            value: Some("/var/lib/kuberic".to_string()),
            ..Default::default()
        },
    ];
    if set
        .annotations()
        .get(LIVE_TEST_COPY_GATE_ANNOTATION)
        .is_some_and(|value| value == "enabled")
    {
        env.push(EnvVar {
            name: "KUBERIC_LIVE_TEST_COPY_GATE_ADDRESS".to_string(),
            value: Some(LIVE_TEST_COPY_GATE_ADDRESS.to_string()),
            ..Default::default()
        });
    }
    Pod {
        metadata: kube::core::ObjectMeta {
            name: Some(pod_name.to_string()),
            labels: Some(labels),
            annotations,
            owner_references: Some(vec![owner.clone()]),
            ..Default::default()
        },
        spec: Some(PodSpec {
            automount_service_account_token: Some(false),
            hostname: Some(pod_name.to_string()),
            subdomain: Some(format!("{}-peer", set.name_any())),
            security_context: Some(PodSecurityContext {
                fs_group: Some(10001),
                run_as_non_root: Some(true),
                run_as_user: Some(10001),
                ..Default::default()
            }),
            containers: vec![Container {
                name: "application".to_string(),
                image: Some(image.to_string()),
                image_pull_policy: Some("IfNotPresent".to_string()),
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
                    ContainerPort {
                        container_port: 8080,
                        name: Some("application".to_string()),
                        ..Default::default()
                    },
                ]),
                env: Some(env),
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

fn effective_replica_image(observation: &RawObservation) -> Result<String> {
    Ok(observation.set.spec.image.clone())
}

#[allow(clippy::too_many_arguments)]
async fn ensure_exact_peer_endpoint(
    client: Client,
    observation: &RawObservation,
    namespace: &str,
    uid: &str,
    owner: &OwnerReference,
    pod: &Pod,
    pvc: &PersistentVolumeClaim,
    replica_id: ReplicaId,
) -> Result<()> {
    let pod_uid = pod.uid().ok_or(ControllerError::ObservationStale)?;
    let pvc_uid = pvc.uid().ok_or(ControllerError::ObservationStale)?;
    let resource_uid = ResourceUid::new(uid);
    let initialization_id = derive_initialization_id(
        &resource_uid,
        replica_id,
        &PodUid::new(&pod_uid),
        &PvcUid::new(&pvc_uid),
    );
    let identity = ReplicaIdentity {
        replica_id,
        instance_id: ReplicaInstanceId::new(&pod_uid),
        agent_generation: derive_agent_generation(&initialization_id),
    };
    if pod.labels().get(INSTANCE_LABEL).map(String::as_str) != Some(pod_uid.as_str()) {
        patch_pod_instance(client.clone(), namespace, pod, &pod_uid).await?;
        return Ok(());
    }
    let name = derive_replica_endpoint_name(&resource_uid, &identity);
    if observation
        .services
        .iter()
        .any(|service| service.name_any() == name)
    {
        return Ok(());
    }
    let services: Api<Service> = Api::namespaced(client, namespace);
    create_exact(
        &services,
        &name,
        &Service {
            metadata: kube::core::ObjectMeta {
                name: Some(name.clone()),
                labels: Some(base_labels(&observation.set, Some(replica_id), uid)),
                owner_references: Some(vec![owner.clone()]),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(BTreeMap::from([(INSTANCE_LABEL.to_string(), pod_uid)])),
                ports: Some(vec![
                    ServicePort {
                        name: Some("control".to_string()),
                        port: CONTROL_PORT,
                        target_port: Some(
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                                CONTROL_PORT,
                            ),
                        ),
                        ..Default::default()
                    },
                    ServicePort {
                        name: Some("replication".to_string()),
                        port: REPLICATION_PORT,
                        target_port: Some(
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                                REPLICATION_PORT,
                            ),
                        ),
                        ..Default::default()
                    },
                ]),
                ..Default::default()
            }),
            ..Default::default()
        },
    )
    .await
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
                    k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(8080),
                ),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    create_exact(&services, &service_name, &service).await
}

fn agent_credential_name(set: &KubericSet) -> String {
    format!("{}-agent-credentials", set.name_any())
}

async fn ensure_agent_credentials(
    client: Client,
    observation: &RawObservation,
    namespace: &str,
    uid: &str,
    owner: &OwnerReference,
    bearer_token: &str,
) -> Result<()> {
    let name = agent_credential_name(&observation.set);
    let secrets: Api<Secret> = Api::namespaced(client, namespace);
    let desired = Secret {
        metadata: kube::core::ObjectMeta {
            name: Some(name.clone()),
            labels: Some(base_labels(&observation.set, None, uid)),
            owner_references: Some(vec![owner.clone()]),
            ..Default::default()
        },
        string_data: Some(BTreeMap::from([(
            "bearer-token".to_string(),
            bearer_token.to_string(),
        )])),
        type_: Some("Opaque".to_string()),
        ..Default::default()
    };
    match secrets
        .get_opt(&name)
        .await
        .map_err(map_kube_effect_error)?
    {
        None => create_exact(&secrets, &name, &desired).await,
        Some(existing) => {
            if existing.labels().get(SET_UID_LABEL).map(String::as_str) != Some(uid) {
                return Err(ControllerError::Effect(format!(
                    "Secret {name} is not owned by this KubericSet"
                )));
            }
            let mut replacement = desired;
            replacement.metadata.resource_version = existing.resource_version();
            secrets
                .replace(&name, &PostParams::default(), &replacement)
                .await
                .map(|_| ())
                .map_err(map_kube_effect_error)
        }
    }
}

async fn ensure_peer_service(
    client: Client,
    observation: &RawObservation,
    namespace: &str,
    uid: &str,
    owner: &OwnerReference,
) -> Result<()> {
    let name = format!("{}-peer", observation.set.name_any());
    if let Some(existing) = observation
        .services
        .iter()
        .find(|service| service.name_any() == name)
    {
        if existing.labels().get(SET_UID_LABEL).map(String::as_str) != Some(uid) {
            return Err(ControllerError::Effect(format!(
                "Service {name} is not owned by this KubericSet"
            )));
        }
        let ready = existing.spec.as_ref().is_some_and(|spec| {
            spec.cluster_ip.as_deref() == Some("None")
                && spec.publish_not_ready_addresses == Some(true)
                && spec.selector.as_ref().is_some_and(|selector| {
                    selector.get(SET_UID_LABEL).map(String::as_str) == Some(uid)
                })
                && spec.ports.as_ref().is_some_and(|ports| {
                    [("control", CONTROL_PORT), ("replication", REPLICATION_PORT)]
                        .into_iter()
                        .all(|(name, port)| {
                            ports.iter().any(|candidate| {
                                candidate.name.as_deref() == Some(name) && candidate.port == port
                            })
                        })
                })
        });
        if ready {
            return Ok(());
        }
        let services: Api<Service> = Api::namespaced(client, namespace);
        services
            .delete(
                &name,
                &DeleteParams {
                    preconditions: Some(Preconditions {
                        uid: existing.uid(),
                        resource_version: existing.resource_version(),
                    }),
                    ..Default::default()
                },
            )
            .await
            .map(|_| ())
            .map_err(map_kube_effect_error)?;
        return Ok(());
    }
    let services: Api<Service> = Api::namespaced(client, namespace);
    let service = Service {
        metadata: kube::core::ObjectMeta {
            name: Some(name.clone()),
            labels: Some(base_labels(&observation.set, None, uid)),
            owner_references: Some(vec![owner.clone()]),
            ..Default::default()
        },
        spec: Some(ServiceSpec {
            cluster_ip: Some("None".to_string()),
            publish_not_ready_addresses: Some(true),
            selector: Some(BTreeMap::from([(
                SET_UID_LABEL.to_string(),
                uid.to_string(),
            )])),
            ports: Some(vec![
                ServicePort {
                    name: Some("control".to_string()),
                    port: CONTROL_PORT,
                    target_port: Some(
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                            CONTROL_PORT,
                        ),
                    ),
                    ..Default::default()
                },
                ServicePort {
                    name: Some("replication".to_string()),
                    port: REPLICATION_PORT,
                    target_port: Some(
                        k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(
                            REPLICATION_PORT,
                        ),
                    ),
                    ..Default::default()
                },
            ]),
            ..Default::default()
        }),
        ..Default::default()
    };
    create_exact(&services, &name, &service).await
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

async fn patch_public_v1_write_service(
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
        {"op": "add", "path": "/spec/selector", "value": {INSTANCE_LABEL: instance}},
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
        kube::Error::Api(response) if matches!(response.code, 409 | 412 | 422) => {
            ControllerError::ObservationStale
        }
        other => ControllerError::Effect(other.to_string()),
    }
}

fn map_kube_observation_error(error: kube::Error) -> ControllerError {
    let transient = match &error {
        kube::Error::Api(response) => response.code == 429 || response.code >= 500,
        kube::Error::HyperError(_) | kube::Error::Service(_) | kube::Error::ReadEvents(_) => true,
        _ => false,
    };
    if transient {
        ControllerError::TransientObservation(error.to_string())
    } else {
        ControllerError::Observation(error.to_string())
    }
}

#[cfg(test)]
#[test]
fn exact_pod_precondition_failures_require_reobservation() {
    for code in [409, 412, 422] {
        let error = kube::Error::Api(Box::new(kube::core::Status {
            message: "precondition changed".into(),
            reason: "Conflict".into(),
            code,
            ..Default::default()
        }));
        assert!(matches!(
            map_kube_effect_error(error),
            ControllerError::ObservationStale
        ));
    }
}

#[cfg(test)]
#[test]
fn observation_http_errors_preserve_permanent_and_transient_classification() {
    for code in [400, 401, 403, 404, 422] {
        let error = kube::Error::Api(Box::new(kube::core::Status {
            message: "permanent observation failure".into(),
            reason: "Permanent".into(),
            code,
            ..Default::default()
        }));
        assert!(
            matches!(
                map_kube_observation_error(error),
                ControllerError::Observation(_)
            ),
            "HTTP {code} must fail immediately"
        );
    }
    for code in [429, 500, 502, 503, 504] {
        let error = kube::Error::Api(Box::new(kube::core::Status {
            message: "transient observation failure".into(),
            reason: "Transient".into(),
            code,
            ..Default::default()
        }));
        assert!(
            matches!(
                map_kube_observation_error(error),
                ControllerError::TransientObservation(_)
            ),
            "HTTP {code} must remain retryable"
        );
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
        ProtocolCommand::PublicV1History(command) => {
            (command.target().clone(), command.target().replica_id)
        }
        ProtocolCommand::PublicV1Operation(command) => {
            (command.target.clone(), command.target.replica_id)
        }
    }
}

fn command_request(
    resource_uid: String,
    target: ReplicaIdentity,
    expected_process_session_id: String,
    command: ProtocolCommand,
) -> Result<proto::ExecuteCommandRequest> {
    let command = match command {
        ProtocolCommand::InitializeAgentStore(command) => {
            proto::execute_command_request::Command::InitializeAgentStore(initialize_command(
                *command,
            ))
        }
        ProtocolCommand::PublicV1History(command) => {
            proto::execute_command_request::Command::PublicV1History(
                kuberic_runtime::control::public_v1_history_command_to_proto(&command)
                    .map_err(|error| ControllerError::Effect(error.to_string()))?,
            )
        }
        ProtocolCommand::PublicV1Operation(command) => {
            proto::execute_command_request::Command::PublicV1Operation(
                kuberic_runtime::control::public_v1_operation_command_to_proto(&command)
                    .map_err(|error| ControllerError::Effect(error.to_string()))?,
            )
        }
    };
    Ok(proto::ExecuteCommandRequest {
        protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
        resource_uid,
        target: Some(target.into()),
        expected_process_session_id,
        command: Some(command),
    })
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
        bootstrap_configuration: Some(command.bootstrap_configuration.into()),
        provisioning: command.provisioning.map(provisioning),
    }
}

fn provisioning(provisioning: ProvisioningIntent) -> proto::ProvisioningIntent {
    use proto::provisioning_intent::Purpose;
    proto::ProvisioningIntent {
        purpose: Some(match provisioning.purpose.kind {
            kuberic_runtime::protocol::types::ProvisioningKind::Replacement => Purpose::Replaces(
                provisioning
                    .purpose
                    .replaces
                    .expect("validated replacement provisioning")
                    .into(),
            ),
            kuberic_runtime::protocol::types::ProvisioningKind::ScaleUp => Purpose::ScaleUp(
                provisioning
                    .purpose
                    .scale_up
                    .expect("validated scale-up provisioning")
                    .into(),
            ),
        }),
        pod_uid: provisioning.pod_uid.to_string(),
        pvc_uid: provisioning.pvc_uid.to_string(),
        operation_id: provisioning.operation_id.to_string(),
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

impl<A> KubeClusterApi<A>
where
    A: AgentApi + 'static,
{
    async fn observe_public_v1_scope(
        &self,
        scope: &PublicV1Scope,
    ) -> std::result::Result<RawObservation, String> {
        self.require_installation_namespace(&scope.namespace)
            .map_err(|error| error.to_string())?;
        let observation = self
            .observe(&scope.namespace, &scope.name)
            .await
            .map_err(|error| error.to_string())?;
        if observation.set.uid().as_deref() != Some(scope.resource_uid.as_str()) {
            return Err("public-v1 scope no longer identifies the observed resource".into());
        }
        validate_public_v1_controller_status(observation.set.status.as_ref())?;
        Ok(observation)
    }

    async fn replace_public_v1_status(
        &self,
        observation: &RawObservation,
        status: KubericSetStatus,
    ) -> std::result::Result<(), String> {
        let namespace = observation
            .set
            .namespace()
            .ok_or_else(|| "KubericSet has no namespace".to_string())?;
        self.require_installation_namespace(&namespace)
            .map_err(|error| error.to_string())?;
        let sets: Api<KubericSet> = Api::namespaced(self.client.clone(), &namespace);
        let mut set = observation.set.clone();
        set.status = Some(status);
        sets.replace_status(&set.name_any(), &PostParams::default(), &set)
            .await
            .map(|_| ())
            .map_err(|error| map_kube_status_error(error).to_string())
    }

    async fn complete_public_v1_transition(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
    ) -> std::result::Result<(), String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        if normalized_transition(&observation)?.as_ref() != Some(expected) {
            return Err("stale public-v1 transition completion".into());
        }
        let mut status = observation.set.status.clone().unwrap_or_default();
        status.public_v1_transition = None;
        status.public_v1_operation = None;
        self.replace_public_v1_status(&observation, status).await
    }
}

fn validate_public_v1_controller_status(
    status: Option<&KubericSetStatus>,
) -> std::result::Result<(), String> {
    status.map_or(Ok(()), KubericSetStatus::validate_public_v1)
}

fn normalized_transition(
    observation: &RawObservation,
) -> std::result::Result<Option<PublicV1AcceptedTransitionStatus>, String> {
    let mut transition = observation
        .set
        .status
        .as_ref()
        .and_then(|status| status.public_v1_transition.as_ref())
        .map(PublicV1TransitionState::transition)
        .transpose()?;
    if let Some(transition) = &mut transition {
        transition.resource_version = observation
            .set
            .resource_version()
            .ok_or_else(|| "KubericSet has no resourceVersion".to_string())?;
        transition.validate()?;
    }
    Ok(transition)
}

fn accepted_public_v1_status(
    status: Option<&KubericSetStatus>,
) -> std::result::Result<Option<PublicV1AcceptedStatus>, String> {
    status
        .and_then(|status| status.public_v1_authority.as_ref())
        .map(PublicV1AuthorityStatus::authority)
        .transpose()
}

fn accepted_public_v1_renewal(
    status: Option<&KubericSetStatus>,
) -> std::result::Result<Option<PublicV1SessionRenewalStatus>, String> {
    status
        .and_then(|status| status.public_v1_session_renewal.as_ref())
        .map(PublicV1SessionRenewalState::renewal)
        .transpose()
}

fn inbound_build_authority(
    expected: &PublicV1AcceptedTransitionStatus,
    operation: &kuberic_runtime::protocol::command::PublicV1OperationCommand,
    build: &kuberic_runtime::protocol::public_operations::PublicBuildInput,
) -> std::result::Result<PublicInboundBuildAuthority, String> {
    let target = expected
        .intent
        .target
        .as_ref()
        .ok_or_else(|| "public-v1 build lacks accepted target authority".to_string())?;
    if target.storage_incarnation != build.target_storage
        || build.replica != build.target_storage.replica
        || build.history.as_ref() != Some(&expected.intent.history)
        || expected.intent.current_configuration_id.is_none()
    {
        return Err("public-v1 build changed target, history, or configuration".into());
    }
    let authority = PublicInboundBuildAuthority {
        operation_id: expected.intent.operation_id.clone(),
        operation_revision: expected.intent.revision,
        attempt: PublicBuildAttemptReference {
            resource_uid: expected.intent.history.resource_uid.clone(),
            epoch: build.epoch,
            build_id: build.attempt.clone(),
            source: operation.target.clone(),
            source_storage: expected.intent.source.storage_incarnation.clone(),
            source_session: operation.intent.process_session_id.clone(),
            target: build.replica.clone(),
            target_storage: build.target_storage.clone(),
            target_session: build.process_session_id.clone(),
        },
        history: expected.intent.history.clone(),
        current_configuration_id: expected
            .intent
            .current_configuration_id
            .clone()
            .expect("checked current configuration"),
        target_address: build.replication_address.clone(),
    };
    authority.validate().map_err(str::to_owned)?;
    Ok(authority)
}

#[async_trait]
impl<A> PublicV1HistoryApi for KubeClusterApi<A>
where
    A: AgentApi + 'static,
{
    async fn observe_public_v1_status(
        &self,
        scope: &PublicV1Scope,
    ) -> std::result::Result<Option<PublicV1AcceptedStatus>, String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        accepted_public_v1_status(observation.set.status.as_ref())
    }

    async fn persist_public_v1_status(
        &self,
        scope: &PublicV1Scope,
        expected: Option<&PublicV1AcceptedStatus>,
        next: &PublicV1AcceptedStatus,
    ) -> std::result::Result<(), String> {
        next.validate_evidence().map_err(str::to_owned)?;
        if next.history.resource_uid != scope.resource_uid {
            return Err("public-v1 history differs from resource scope".into());
        }
        let observation = self.observe_public_v1_scope(scope).await?;
        let mut status = observation.set.status.clone().unwrap_or_default();
        if accepted_public_v1_status(Some(&status))?.as_ref() != expected {
            return Err("stale public-v1 accepted history".into());
        }
        if let Some(transition_state) = status.public_v1_transition.as_mut() {
            let mut transition = transition_state.transition()?;
            if transition.evidence
                != *expected.ok_or_else(|| {
                    "public-v1 transition exists before accepted history".to_string()
                })?
            {
                return Err("public-v1 transition changed accepted history".into());
            }
            transition.evidence = next.clone();
            *transition_state = PublicV1TransitionState::from_transition(&transition)?;
        }
        status.public_v1_authority = Some(PublicV1AuthorityStatus::from_authority(next)?);
        self.replace_public_v1_status(&observation, status).await
    }

    async fn dispatch_public_v1_history(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedStatus,
        command: &ProtocolCommand,
    ) -> std::result::Result<(), String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        if accepted_public_v1_status(observation.set.status.as_ref())?.as_ref() != Some(expected) {
            return Err("stale public-v1 history dispatch".into());
        }
        self.execute_command(&observation, command)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
impl<A> PublicV1SessionRenewalApi for KubeClusterApi<A>
where
    A: AgentApi + 'static,
{
    async fn observe_public_v1_session_renewal(
        &self,
        scope: &PublicV1Scope,
    ) -> std::result::Result<Option<PublicV1SessionRenewalStatus>, String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        accepted_public_v1_renewal(observation.set.status.as_ref())
    }

    async fn persist_public_v1_session_renewal(
        &self,
        scope: &PublicV1Scope,
        expected: Option<&PublicV1SessionRenewalStatus>,
        next: &PublicV1SessionRenewalStatus,
    ) -> std::result::Result<(), String> {
        next.validate().map_err(str::to_owned)?;
        let observation = self.observe_public_v1_scope(scope).await?;
        let mut status = observation.set.status.clone().unwrap_or_default();
        if accepted_public_v1_renewal(Some(&status))?.as_ref() != expected {
            return Err("stale public-v1 session renewal".into());
        }
        status.public_v1_session_renewal = Some(PublicV1SessionRenewalState::from_renewal(next)?);
        self.replace_public_v1_status(&observation, status).await
    }

    async fn dispatch_public_v1_session_renewal(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1SessionRenewalStatus,
        command: &ProtocolCommand,
    ) -> std::result::Result<(), String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        if accepted_public_v1_renewal(observation.set.status.as_ref())?.as_ref() != Some(expected) {
            return Err("stale public-v1 session-renewal dispatch".into());
        }
        self.execute_command(&observation, command)
            .await
            .map_err(|error| error.to_string())?;
        let ProtocolCommand::PublicV1History(public_command) = command else {
            return Err("session renewal received another command".into());
        };
        let kuberic_runtime::protocol::command::PublicV1HistoryCommand::SessionRenewal(
            renewal_command,
        ) = public_command.as_ref()
        else {
            return Err("session renewal received a history command".into());
        };
        let next = match renewal_command.as_ref() {
            kuberic_runtime::protocol::command::PublicV1SessionRenewalCommand::CloseAccessAndIngress {
                accepted,
            } => accepted.close_access(),
            kuberic_runtime::protocol::command::PublicV1SessionRenewalCommand::UpdateEpochBarrier {
                accepted,
                survivor,
                epoch,
            } => accepted.record_epoch_updated(survivor, *epoch),
            kuberic_runtime::protocol::command::PublicV1SessionRenewalCommand::ReplayReplacementRole {
                accepted,
                process_session_id,
                epoch,
                role,
            } => accepted.replay_replacement_role(process_session_id, *epoch, *role),
            kuberic_runtime::protocol::command::PublicV1SessionRenewalCommand::InstallFreshConfiguration {
                accepted,
                configuration,
            } => accepted.install_fresh_configuration(configuration),
        }
        .map_err(str::to_owned)?;
        let mut status = observation.set.status.clone().unwrap_or_default();
        status.public_v1_session_renewal = Some(PublicV1SessionRenewalState::from_renewal(&next)?);
        self.replace_public_v1_status(&observation, status).await
    }
}

#[async_trait]
impl<A> PublicV1OperationApi for KubeClusterApi<A>
where
    A: AgentApi + 'static,
{
    async fn observe_public_v1_topology(
        &self,
        scope: &PublicV1Scope,
    ) -> std::result::Result<Option<ConfigurationDescriptor>, String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        observation
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .map(PublicV1TopologyState::configuration)
            .transpose()
    }

    async fn persist_public_v1_topology(
        &self,
        scope: &PublicV1Scope,
        expected: Option<&kuberic_runtime::protocol::types::ConfigurationId>,
        next: &ConfigurationDescriptor,
    ) -> std::result::Result<(), String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        let mut status = observation.set.status.clone().unwrap_or_default();
        let current = status
            .public_v1_topology
            .as_ref()
            .map(PublicV1TopologyState::configuration)
            .transpose()?;
        if current
            .as_ref()
            .map(|configuration| &configuration.configuration_id)
            != expected
        {
            return Err("stale public-v1 topology persistence".into());
        }
        if status.public_v1_transition.is_some() || status.public_v1_operation.is_some() {
            return Err("public-v1 topology cannot change while an operation is pending".into());
        }
        status.public_v1_topology = Some(PublicV1TopologyState::from_configuration(next)?);
        self.replace_public_v1_status(&observation, status).await
    }

    async fn begin_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
        expected_authority: &PublicV1AcceptedStatus,
        transition: &PublicV1AcceptedTransitionStatus,
        command: &kuberic_runtime::protocol::command::PublicV1OperationCommand,
    ) -> std::result::Result<(), String> {
        transition.validate()?;
        command.validate().map_err(str::to_owned)?;
        if &transition.evidence != expected_authority
            || !public_v1_operations::operation_matches_transition(transition, &command.intent)
            || transition.intent.source.storage_incarnation.replica != command.target
        {
            return Err("public-v1 operation differs from proposed transition".into());
        }
        let observation = self.observe_public_v1_scope(scope).await?;
        let mut status = observation.set.status.clone().unwrap_or_default();
        if accepted_public_v1_status(Some(&status))?.as_ref() != Some(expected_authority)
            || status.public_v1_transition.is_some()
            || status.public_v1_operation.is_some()
        {
            return Err("stale public-v1 operation proposal".into());
        }
        status.public_v1_transition = Some(PublicV1TransitionState::from_transition(transition)?);
        status.public_v1_operation = Some(PublicV1OperationStatus::from_command(command)?);
        self.replace_public_v1_status(&observation, status).await
    }

    async fn observe_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
    ) -> std::result::Result<Option<PublicV1AcceptedTransitionStatus>, String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        normalized_transition(&observation)
    }

    async fn dispatch_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        command: &ProtocolCommand,
    ) -> std::result::Result<(), String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        if normalized_transition(&observation)?.as_ref() != Some(expected) {
            return Err("stale public-v1 operation dispatch".into());
        }
        if let ProtocolCommand::PublicV1Operation(operation) = command
            && let Some(PublicOperationProgram::Build(build)) = operation.intent.program.as_ref()
        {
            let authority = inbound_build_authority(expected, operation, build)?;
            self.execute_command(
                &observation,
                &ProtocolCommand::PublicV1History(Box::new(
                    kuberic_runtime::protocol::command::PublicV1HistoryCommand::
                        InstallInboundBuildAuthority {
                            authority: Box::new(authority),
                        },
                )),
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        self.execute_command(&observation, command)
            .await
            .map_err(|error| error.to_string())
    }

    async fn observe_public_v1_operation_report(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
    ) -> std::result::Result<PublicV1AgentReport, String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        if normalized_transition(&observation)?.as_ref() != Some(expected) {
            return Err("stale public-v1 operation report authority".into());
        }
        let identity = &expected.intent.source.storage_incarnation.replica;
        let key = ReplicaObservationKey::new(identity.replica_id, identity.instance_id.clone());
        match observation.agents.get(&key) {
            Some(RawAgentObservation::PublicV1Report(report))
                if report.replica == *identity
                    && report.process_session_id == expected.intent.process_session_id =>
            {
                Ok((**report).clone())
            }
            _ => Err("exact public-v1 operation report is unavailable".into()),
        }
    }
}

#[async_trait]
impl<A> PublicV1EvidenceApi for KubeClusterApi<A>
where
    A: AgentApi + 'static,
{
    async fn observe_public_v1_evidence_status(
        &self,
        scope: &PublicV1Scope,
    ) -> std::result::Result<Option<PublicV1AcceptedTransitionStatus>, String> {
        self.observe_public_v1_operation(scope).await
    }

    async fn persist_public_v1_evidence_status(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        next: &PublicV1AcceptedStatus,
    ) -> std::result::Result<(), String> {
        if !next.is_evidence_successor_of(&expected.evidence) {
            return Err("public-v1 evidence persistence is not monotonic".into());
        }
        let observation = self.observe_public_v1_scope(scope).await?;
        if normalized_transition(&observation)?.as_ref() != Some(expected) {
            return Err("stale public-v1 evidence persistence".into());
        }
        let mut status = observation.set.status.clone().unwrap_or_default();
        let transition_state = status
            .public_v1_transition
            .as_mut()
            .ok_or_else(|| "public-v1 transition disappeared".to_string())?;
        let mut transition = transition_state.transition()?;
        transition.evidence = next.clone();
        *transition_state = PublicV1TransitionState::from_transition(&transition)?;
        status.public_v1_authority = Some(PublicV1AuthorityStatus::from_authority(next)?);
        self.replace_public_v1_status(&observation, status).await
    }

    async fn accept_public_v1_completion(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
    ) -> std::result::Result<(), String> {
        let observed = self
            .observe_public_v1_evidence_status(scope)
            .await?
            .ok_or_else(|| "public-v1 transition disappeared".to_string())?;
        if &observed != expected || !expected.intent.matches(completion) {
            return Err("stale public-v1 completion acceptance".into());
        }
        if completion.result != PublicV1OperationResult::Succeeded {
            return Err("public-v1 acceptance requires successful completion".into());
        }
        let observation = self.observe_public_v1_scope(scope).await?;
        if let Some(operation) = observation
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_operation.as_ref())
            .map(PublicV1OperationStatus::command)
            .transpose()?
            && let Some(PublicOperationProgram::Build(build)) = operation.intent.program.as_ref()
        {
            let authority = inbound_build_authority(expected, &operation, build)?;
            self.execute_command(
                &observation,
                &ProtocolCommand::PublicV1History(Box::new(
                    kuberic_runtime::protocol::command::PublicV1HistoryCommand::
                        RevokeInboundBuildAuthority {
                            authority: Box::new(authority),
                        },
                )),
            )
            .await
            .map_err(|error| error.to_string())?;
        }
        self.complete_public_v1_transition(scope, expected).await
    }

    async fn retire_public_v1_incarnation(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
        target: &PublicV1ResourceAuthority,
    ) -> std::result::Result<(), String> {
        if !expected.intent.matches(completion) || target != expected.intent.effect_target() {
            return Err("public-v1 retirement differs from accepted evidence".into());
        }
        let observation = self.observe_public_v1_scope(scope).await?;
        if normalized_transition(&observation)?.as_ref() != Some(expected) {
            return Err("stale public-v1 retirement authority".into());
        }
        let storage = &target.storage_incarnation;
        let (pod_uid, pvc_uid) = match &storage.storage {
            kuberic_runtime::protocol::types::ReplicaStorageIdentity::Persistent {
                pod_uid,
                pvc_uid,
            } => (Some(pod_uid), Some(pvc_uid)),
            kuberic_runtime::protocol::types::ReplicaStorageIdentity::Volatile { .. } => {
                (None, None)
            }
        };
        let pod_name = pod_uid.and_then(|uid| {
            observation
                .pods
                .iter()
                .find(|pod| pod.uid().as_deref() == Some(uid.as_str()))
                .map(ResourceExt::name_any)
        });
        let pvc_name = pvc_uid.and_then(|uid| {
            observation
                .pvcs
                .iter()
                .find(|pvc| pvc.uid().as_deref() == Some(uid.as_str()))
                .map(ResourceExt::name_any)
        });
        self.delete_replica_scaffolding(
            &observation,
            pod_name.as_deref(),
            pod_uid,
            pvc_name.as_deref(),
            pvc_uid,
        )
        .await
        .map_err(|error| error.to_string())?;
        self.complete_public_v1_transition(scope, expected).await
    }

    async fn provision_public_v1_replacement(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
        target: &PublicV1ResourceAuthority,
        rebuild: &CanonicalReplicaRebuildRequired,
    ) -> std::result::Result<(), String> {
        if !expected.intent.matches(completion)
            || !matches!(
                &completion.result,
                PublicV1OperationResult::RebuildRequired(observed) if observed.as_ref() == rebuild
            )
        {
            return Err("public-v1 replacement differs from accepted evidence".into());
        }
        let observation = self.observe_public_v1_scope(scope).await?;
        if normalized_transition(&observation)?.as_ref() != Some(expected) {
            return Err("stale public-v1 replacement authority".into());
        }
        self.ensure_replacement_scaffolding(
            &observation,
            target.storage_incarnation.replica.replica_id,
            &target.storage_incarnation.replica,
        )
        .await
        .map_err(|error| error.to_string())?;
        self.complete_public_v1_transition(scope, expected).await
    }
}

#[derive(Clone)]
#[cfg(feature = "runtime-test-bridge")]
pub struct InMemoryClusterApi {
    state: Arc<Mutex<InMemoryState>>,
}

#[cfg(feature = "runtime-test-bridge")]
struct InMemoryState {
    observation: RawObservation,
    effects: Vec<EffectRecord>,
    conflict_next_status: bool,
    observation_count: usize,
    active_observations: usize,
    max_active_observations: usize,
    observation_delay: Duration,
    unavailable_next_execute: bool,
    lost_next_create_reply: bool,
    next_resource_uid: u64,
}

#[cfg(feature = "runtime-test-bridge")]
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
                unavailable_next_execute: false,
                lost_next_create_reply: false,
                next_resource_uid: 1,
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

    pub async fn unavailable_next_execute(&self) {
        self.state.lock().await.unavailable_next_execute = true;
    }

    pub async fn lose_next_create_reply(&self) {
        self.state.lock().await.lost_next_create_reply = true;
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
#[cfg(feature = "runtime-test-bridge")]
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
        let mut raw = state.observation.clone();
        let uid = raw.set.uid().unwrap_or_default();
        raw.pods
            .retain(|p| p.labels().get(SET_UID_LABEL) == Some(&uid));
        raw.pvcs
            .retain(|p| p.labels().get(SET_UID_LABEL) == Some(&uid));
        raw.services
            .retain(|p| p.labels().get(SET_UID_LABEL) == Some(&uid));
        Ok(raw)
    }

    async fn ensure_replica_scaffolding(
        &self,
        observation: &RawObservation,
        replica_ids: &[ReplicaId],
    ) -> Result<()> {
        let uid = observation
            .set
            .uid()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no UID".to_string()))?;
        let owner = owner_reference(&observation.set)?;
        let image = effective_replica_image(observation)?;
        let mut state = self.state.lock().await;
        if state.observation.set.resource_version() != observation.set.resource_version() {
            return Err(ControllerError::ObservationStale);
        }
        for replica_id in replica_ids {
            let pod_name = replica_name(&observation.set, *replica_id);
            let pvc_name = format!("{pod_name}-data");
            let Some(pvc) = state
                .observation
                .pvcs
                .iter()
                .find(|pvc| pvc.name_any() == pvc_name)
                .cloned()
            else {
                let resource_number = state.next_resource_uid;
                state.next_resource_uid += 1;
                let mut pvc = replica_pvc(&observation.set, *replica_id, &uid, &owner);
                pvc.metadata.namespace = observation.set.namespace();
                pvc.metadata.uid = Some(format!(
                    "in-memory-pvc-{}-{resource_number}",
                    replica_id.value()
                ));
                pvc.metadata.resource_version = Some(resource_number.to_string());
                state.observation.pvcs.push(pvc);
                state
                    .effects
                    .push(EffectRecord::EnsureScaffolding(replica_ids.to_vec()));
                if state.lost_next_create_reply {
                    state.lost_next_create_reply = false;
                    return Err(ControllerError::ObservationStale);
                }
                return Ok(());
            };
            let pvc_uid = pvc.uid().ok_or(ControllerError::ObservationStale)?;
            let Some(pod) = state
                .observation
                .pods
                .iter()
                .find(|pod| pod.name_any() == pod_name)
                .cloned()
            else {
                let resource_number = state.next_resource_uid;
                state.next_resource_uid += 1;
                let mut pod = replica_pod(
                    &observation.set,
                    *replica_id,
                    &uid,
                    &owner,
                    &pvc_name,
                    &pvc_uid,
                    &image,
                );
                let pod_uid = format!("in-memory-pod-{}-{resource_number}", replica_id.value());
                pod.metadata.namespace = observation.set.namespace();
                pod.metadata.uid = Some(pod_uid.clone());
                pod.metadata.resource_version = Some(resource_number.to_string());
                pod.metadata.annotations.get_or_insert_default().insert(
                    CONTROL_ADDRESS_ANNOTATION.to_string(),
                    "http://127.0.0.1:50051".to_string(),
                );
                pod.status = Some(k8s_openapi::api::core::v1::PodStatus {
                    pod_ip: Some("127.0.0.1".to_string()),
                    conditions: Some(vec![k8s_openapi::api::core::v1::PodCondition {
                        type_: "Ready".to_string(),
                        status: "True".to_string(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                });
                state.observation.pods.push(pod);
                state
                    .observation
                    .agents
                    .entry(ReplicaObservationKey::new(
                        *replica_id,
                        ReplicaInstanceId::new(&pod_uid),
                    ))
                    .or_insert_with(|| {
                        RawAgentObservation::Uninitialized(
                            kuberic_runtime::protocol::observation::UninitializedAgentObservation {
                                protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                                resource_uid: ResourceUid::new(&uid),
                                replica_id: *replica_id,
                                pod_uid: PodUid::new(&pod_uid),
                                pvc_uid: PvcUid::new(&pvc_uid),
                                process_session_id:
                                    kuberic_runtime::protocol::types::ProcessSessionId::new(
                                        format!(
                                            "in-memory-session-{}-{resource_number}",
                                            replica_id.value()
                                        ),
                                    ),
                                report_sequence: 1,
                            },
                        )
                    });
                state
                    .effects
                    .push(EffectRecord::EnsureScaffolding(replica_ids.to_vec()));
                if state.lost_next_create_reply {
                    state.lost_next_create_reply = false;
                    return Err(ControllerError::ObservationStale);
                }
                return Ok(());
            };
            let pod_uid = pod.uid().ok_or(ControllerError::ObservationStale)?;
            if pod.labels().get(INSTANCE_LABEL).map(String::as_str) != Some(pod_uid.as_str()) {
                state
                    .observation
                    .pods
                    .iter_mut()
                    .find(|candidate| candidate.name_any() == pod_name)
                    .expect("observed Pod remains present")
                    .metadata
                    .labels
                    .get_or_insert_default()
                    .insert(INSTANCE_LABEL.to_string(), pod_uid.clone());
                state
                    .effects
                    .push(EffectRecord::EnsureScaffolding(replica_ids.to_vec()));
                return Ok(());
            }
            let initialization_id = derive_initialization_id(
                &ResourceUid::new(&uid),
                *replica_id,
                &PodUid::new(&pod_uid),
                &PvcUid::new(&pvc_uid),
            );
            let target = ReplicaIdentity {
                replica_id: *replica_id,
                instance_id: ReplicaInstanceId::new(&pod_uid),
                agent_generation: derive_agent_generation(&initialization_id),
            };
            let endpoint_name = derive_replica_endpoint_name(&ResourceUid::new(&uid), &target);
            if !state
                .observation
                .services
                .iter()
                .any(|service| service.name_any() == endpoint_name)
            {
                let resource_number = state.next_resource_uid;
                state.next_resource_uid += 1;
                state.observation.services.push(Service {
                    metadata: kube::core::ObjectMeta {
                        name: Some(endpoint_name),
                        namespace: observation.set.namespace(),
                        uid: Some(format!(
                            "in-memory-endpoint-{}-{resource_number}",
                            replica_id.value()
                        )),
                        resource_version: Some(resource_number.to_string()),
                        labels: Some(base_labels(&observation.set, Some(*replica_id), &uid)),
                        owner_references: Some(vec![owner.clone()]),
                        ..Default::default()
                    },
                    spec: Some(ServiceSpec {
                        selector: Some(BTreeMap::from([(INSTANCE_LABEL.to_string(), pod_uid)])),
                        ports: Some(vec![
                            ServicePort {
                                name: Some("control".to_string()),
                                port: CONTROL_PORT,
                                ..Default::default()
                            },
                            ServicePort {
                                name: Some("replication".to_string()),
                                port: REPLICATION_PORT,
                                ..Default::default()
                            },
                        ]),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
        }
        let write_service_name = format!("{}-write", observation.set.name_any());
        if !state
            .observation
            .services
            .iter()
            .any(|service| service.name_any() == write_service_name)
        {
            let resource_number = state.next_resource_uid;
            state.next_resource_uid += 1;
            state.observation.services.push(Service {
                metadata: kube::core::ObjectMeta {
                    name: Some(write_service_name),
                    namespace: observation.set.namespace(),
                    uid: Some(format!("in-memory-write-service-{resource_number}")),
                    resource_version: Some(resource_number.to_string()),
                    labels: Some(base_labels(&observation.set, None, &uid)),
                    owner_references: Some(vec![owner]),
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
                        ..Default::default()
                    }]),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        state
            .effects
            .push(EffectRecord::EnsureScaffolding(replica_ids.to_vec()));
        Ok(())
    }

    async fn ensure_replica_support(&self, observation: &RawObservation) -> Result<()> {
        let mut state = self.state.lock().await;
        let uid = observation
            .set
            .uid()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no UID".to_string()))?;
        let peer_name = format!("{}-peer", observation.set.name_any());
        if !state
            .observation
            .services
            .iter()
            .any(|service| service.name_any() == peer_name)
        {
            state.observation.services.push(Service {
                metadata: kube::core::ObjectMeta {
                    name: Some(peer_name),
                    labels: Some(BTreeMap::from([(SET_UID_LABEL.to_string(), uid.clone())])),
                    ..Default::default()
                },
                spec: Some(ServiceSpec {
                    cluster_ip: Some("None".to_string()),
                    publish_not_ready_addresses: Some(true),
                    selector: Some(BTreeMap::from([(SET_UID_LABEL.to_string(), uid.clone())])),
                    ports: Some(vec![
                        ServicePort {
                            name: Some("control".to_string()),
                            port: CONTROL_PORT,
                            ..Default::default()
                        },
                        ServicePort {
                            name: Some("replication".to_string()),
                            port: REPLICATION_PORT,
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        let credential_name = format!("{}-agent-credentials", observation.set.name_any());
        if !state
            .observation
            .secrets
            .iter()
            .any(|secret| secret.name_any() == credential_name)
        {
            state.observation.secrets.push(Secret {
                metadata: kube::core::ObjectMeta {
                    name: Some(credential_name),
                    labels: Some(BTreeMap::from([(SET_UID_LABEL.to_string(), uid)])),
                    ..Default::default()
                },
                data: Some(BTreeMap::from([(
                    "bearer-token".to_string(),
                    k8s_openapi::ByteString(b"test-token".to_vec()),
                )])),
                ..Default::default()
            });
        }
        state.effects.push(EffectRecord::EnsureReplicaSupport);
        Ok(())
    }

    async fn converge_public_v1_write_service(
        &self,
        _observation: &RawObservation,
        primary: Option<&ReplicaIdentity>,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        let selector = primary
            .map(|primary| primary.instance_id.to_string())
            .unwrap_or_else(|| "disabled".to_string());
        if let Some(primary) = primary
            && let Some(pod) = state
                .observation
                .pods
                .iter_mut()
                .find(|pod| pod.uid().as_deref() == Some(primary.instance_id.as_str()))
        {
            pod.metadata
                .labels
                .get_or_insert_default()
                .insert(INSTANCE_LABEL.to_string(), selector.clone());
        }
        let service = state
            .observation
            .services
            .iter_mut()
            .find(|service| service.name_any().ends_with("-write"))
            .ok_or(ControllerError::ObservationStale)?;
        service.spec.get_or_insert_default().selector =
            Some(BTreeMap::from([(INSTANCE_LABEL.to_string(), selector)]));
        Ok(())
    }

    async fn ensure_replacement_scaffolding(
        &self,
        observation: &RawObservation,
        replica_id: ReplicaId,
        replacing: &ReplicaIdentity,
    ) -> Result<()> {
        let uid = observation
            .set
            .uid()
            .ok_or_else(|| ControllerError::Effect("KubericSet has no UID".to_string()))?;
        let owner = owner_reference(&observation.set)?;
        let image = effective_replica_image(observation)?;
        let base = derive_replacement_resource_name(&ResourceUid::new(&uid), replacing);
        let pod_name = format!("{}-{base}", observation.set.name_any());
        let pvc_name = format!("{pod_name}-data");
        let mut state = self.state.lock().await;
        if state.observation.set.resource_version() != observation.set.resource_version() {
            return Err(ControllerError::ObservationStale);
        }
        if !state
            .observation
            .pvcs
            .iter()
            .any(|pvc| pvc.name_any() == pvc_name)
        {
            let resource_number = state.next_resource_uid;
            state.next_resource_uid += 1;
            let mut pvc = replica_pvc_named(&observation.set, replica_id, &uid, &owner, &pvc_name);
            pvc.metadata.namespace = observation.set.namespace();
            pvc.metadata.uid = Some(format!(
                "in-memory-replacement-pvc-{}-{resource_number}",
                replica_id.value()
            ));
            pvc.metadata.resource_version = Some(resource_number.to_string());
            state.observation.pvcs.push(pvc);
            state
                .effects
                .push(EffectRecord::EnsureReplacement(replacing.clone()));
            return Ok(());
        }
        let pvc = state
            .observation
            .pvcs
            .iter()
            .find(|pvc| pvc.name_any() == pvc_name)
            .cloned()
            .expect("replacement PVC remains present");
        let pvc_uid = pvc.uid().ok_or(ControllerError::ObservationStale)?;
        if !state
            .observation
            .pods
            .iter()
            .any(|pod| pod.name_any() == pod_name)
        {
            let resource_number = state.next_resource_uid;
            state.next_resource_uid += 1;
            let mut pod = replica_pod_named(
                &observation.set,
                replica_id,
                &uid,
                &owner,
                &pod_name,
                &pvc_name,
                &pvc_uid,
                &image,
                None,
            );
            let pod_uid = format!(
                "in-memory-replacement-pod-{}-{resource_number}",
                replica_id.value()
            );
            pod.metadata.namespace = observation.set.namespace();
            pod.metadata.uid = Some(pod_uid.clone());
            pod.metadata.resource_version = Some(resource_number.to_string());
            pod.metadata
                .labels
                .get_or_insert_default()
                .insert(INSTANCE_LABEL.to_string(), pod_uid.clone());
            pod.metadata.annotations.get_or_insert_default().insert(
                CONTROL_ADDRESS_ANNOTATION.to_string(),
                "http://127.0.0.1:50051".to_string(),
            );
            pod.status = Some(k8s_openapi::api::core::v1::PodStatus {
                pod_ip: Some("127.0.0.1".to_string()),
                conditions: Some(vec![k8s_openapi::api::core::v1::PodCondition {
                    type_: "Ready".to_string(),
                    status: "True".to_string(),
                    ..Default::default()
                }]),
                ..Default::default()
            });
            state.observation.pods.push(pod);
            state.observation.agents.insert(
                ReplicaObservationKey::new(replica_id, ReplicaInstanceId::new(&pod_uid)),
                RawAgentObservation::Uninitialized(
                    kuberic_runtime::protocol::observation::UninitializedAgentObservation {
                        protocol_version: kuberic_runtime::protocol::PROTOCOL_VERSION,
                        resource_uid: ResourceUid::new(&uid),
                        replica_id,
                        pod_uid: PodUid::new(&pod_uid),
                        pvc_uid: PvcUid::new(&pvc_uid),
                        process_session_id: kuberic_runtime::protocol::types::ProcessSessionId::new(
                            format!(
                                "in-memory-replacement-session-{}-{resource_number}",
                                replica_id.value()
                            ),
                        ),
                        report_sequence: 1,
                    },
                ),
            );
            state
                .effects
                .push(EffectRecord::EnsureReplacement(replacing.clone()));
            return Ok(());
        }
        let pod = state
            .observation
            .pods
            .iter()
            .find(|pod| pod.name_any() == pod_name)
            .cloned()
            .expect("replacement Pod remains present");
        let pod_uid = pod.uid().ok_or(ControllerError::ObservationStale)?;
        let initialization_id = derive_initialization_id(
            &ResourceUid::new(&uid),
            replica_id,
            &PodUid::new(&pod_uid),
            &PvcUid::new(&pvc_uid),
        );
        let target = ReplicaIdentity {
            replica_id,
            instance_id: ReplicaInstanceId::new(&pod_uid),
            agent_generation: derive_agent_generation(&initialization_id),
        };
        let endpoint_name = derive_replica_endpoint_name(&ResourceUid::new(&uid), &target);
        if !state
            .observation
            .services
            .iter()
            .any(|service| service.name_any() == endpoint_name)
        {
            let resource_number = state.next_resource_uid;
            state.next_resource_uid += 1;
            state.observation.services.push(Service {
                metadata: kube::core::ObjectMeta {
                    name: Some(endpoint_name),
                    namespace: observation.set.namespace(),
                    uid: Some(format!(
                        "in-memory-replacement-endpoint-{}-{resource_number}",
                        replica_id.value()
                    )),
                    resource_version: Some(resource_number.to_string()),
                    labels: Some(base_labels(&observation.set, Some(replica_id), &uid)),
                    owner_references: Some(vec![owner]),
                    ..Default::default()
                },
                spec: Some(ServiceSpec {
                    selector: Some(BTreeMap::from([(INSTANCE_LABEL.to_string(), pod_uid)])),
                    ports: Some(vec![
                        ServicePort {
                            name: Some("control".to_string()),
                            port: CONTROL_PORT,
                            ..Default::default()
                        },
                        ServicePort {
                            name: Some("replication".to_string()),
                            port: REPLICATION_PORT,
                            ..Default::default()
                        },
                    ]),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
        state
            .effects
            .push(EffectRecord::EnsureReplacement(replacing.clone()));
        Ok(())
    }

    async fn delete_replica_scaffolding(
        &self,
        observation: &RawObservation,
        pod_name: Option<&str>,
        pod_uid: Option<&PodUid>,
        pvc_name: Option<&str>,
        pvc_uid: Option<&PvcUid>,
    ) -> Result<()> {
        let service = pod_uid.and_then(|pod_uid| {
            observation
                .services
                .iter()
                .find(|service| {
                    service.spec.as_ref().is_some_and(|spec| {
                        spec.selector.as_ref().is_some_and(|selector| {
                            selector.get(INSTANCE_LABEL).map(String::as_str)
                                == Some(pod_uid.as_str())
                        })
                    })
                })
                .cloned()
        });
        let mut state = self.state.lock().await;
        if let Some(service) = service.as_ref() {
            let name = service.name_any();
            let uid = service.uid().ok_or(ControllerError::ObservationStale)?;
            state.observation.services.retain(|candidate| {
                candidate.name_any() != name || candidate.uid().as_deref() != Some(uid.as_str())
            });
        }
        if let (Some(name), Some(uid)) = (pod_name, pod_uid) {
            if state
                .observation
                .pods
                .iter()
                .find(|pod| pod.name_any() == name)
                .is_some_and(|pod| pod.uid().as_deref() != Some(uid.as_str()))
            {
                return Err(ControllerError::ObservationStale);
            }
            state
                .observation
                .pods
                .retain(|pod| pod.name_any() != name || pod.uid().as_deref() != Some(uid.as_str()));
        } else if let Some(name) = pod_name {
            state.observation.pods.retain(|pod| pod.name_any() != name);
        }
        if let (Some(name), Some(uid)) = (pvc_name, pvc_uid) {
            if state
                .observation
                .pvcs
                .iter()
                .find(|pvc| pvc.name_any() == name)
                .is_some_and(|pvc| pvc.uid().as_deref() != Some(uid.as_str()))
            {
                return Err(ControllerError::ObservationStale);
            }
            state
                .observation
                .pvcs
                .retain(|pvc| pvc.name_any() != name || pvc.uid().as_deref() != Some(uid.as_str()));
        } else if let Some(name) = pvc_name {
            state.observation.pvcs.retain(|pvc| pvc.name_any() != name);
        }
        state.effects.push(EffectRecord::DeleteScaffolding {
            pod_name: pod_name.map(ToString::to_string),
            pvc_name: pvc_name.map(ToString::to_string),
        });
        Ok(())
    }

    async fn execute_command(
        &self,
        observation: &RawObservation,
        command: &ProtocolCommand,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        let (target, replica_id) = command_target(command);
        let session = observed_process_session(observation, replica_id, &target)?;
        if observed_process_session(&state.observation, replica_id, &target)? != session {
            return Err(ControllerError::ObservationStale);
        }
        kuberic_runtime::control::validate_execute_request(&command_request(
            observation
                .set
                .uid()
                .ok_or(ControllerError::ObservationStale)?,
            target,
            session,
            command.clone(),
        )?)
        .map_err(|e| ControllerError::InvalidAgentEvidence(e.to_string()))?;
        apply_in_memory_command(&mut state.observation, command)?;
        state.effects.push(EffectRecord::Execute(command.clone()));
        let response_lost = state.unavailable_next_execute;
        if response_lost {
            state.unavailable_next_execute = false;
        }

        #[cfg(feature = "runtime-test-bridge")]
        fn apply_in_memory_command(
            observation: &mut RawObservation,
            command: &ProtocolCommand,
        ) -> Result<()> {
            match command {
                ProtocolCommand::InitializeAgentStore(initialization) => {
                    let key = observation
                        .agents
                        .iter()
                        .find_map(|(key, agent)| {
                            matches!(
                                agent,
                                RawAgentObservation::Uninitialized(report)
                                    if report.replica_id == initialization.local_replica_id
                                        && report.process_session_id
                                            == observed_process_session(
                                                observation,
                                                initialization.local_replica_id,
                                                &ReplicaIdentity {
                                                    replica_id: initialization.local_replica_id,
                                                    instance_id: initialization.expected_instance_id.clone(),
                                                    agent_generation: initialization
                                                        .assigned_agent_generation
                                                        .clone(),
                                                },
                                            )
                                            .ok()
                                            .map(kuberic_runtime::protocol::types::ProcessSessionId::new)
                                            .unwrap_or_default()
                            )
                            .then_some(key.clone())
                        })
                        .ok_or(ControllerError::ObservationStale)?;
                    let RawAgentObservation::Uninitialized(fresh) = observation
                        .agents
                        .get(&key)
                        .cloned()
                        .ok_or(ControllerError::ObservationStale)?
                    else {
                        unreachable!()
                    };
                    let generation =
                        kuberic_runtime::protocol::types::PublicProtocolGeneration::new(1);
                    let history_initialization_id =
                        kuberic_runtime::protocol::types::derive_history_initialization_id(
                            &initialization.resource_uid,
                            generation,
                        );
                    let history = kuberic_runtime::protocol::types::derive_bootstrap_history(
                        &initialization.resource_uid,
                        generation,
                        &history_initialization_id,
                    );
                    let replica = ReplicaIdentity {
                        replica_id: initialization.local_replica_id,
                        instance_id: initialization.expected_instance_id.clone(),
                        agent_generation: initialization.assigned_agent_generation.clone(),
                    };
                    observation.agents.insert(
                        key,
                        RawAgentObservation::PublicV1Report(Box::new(PublicV1AgentReport {
                            identity:
                                kuberic_runtime::protocol::public_operations::PublicV1Identity::new(1),
                            resource_uid: initialization.resource_uid.clone(),
                            replica: replica.clone(),
                            process_session_id: fresh.process_session_id,
                            report_sequence: 1,
                            role: ReplicaRole::None,
                            read_status: kuberic_runtime::protocol::types::AccessStatus::NotPrimary,
                            write_status: kuberic_runtime::protocol::types::AccessStatus::NotPrimary,
                            history,
                            storage_incarnation:
                                kuberic_runtime::protocol::types::ReplicaStorageIncarnationContext {
                                    replica,
                                    storage:
                                        kuberic_runtime::protocol::types::ReplicaStorageIdentity::Persistent {
                                            pod_uid: initialization.expected_pod_uid.clone(),
                                            pvc_uid: initialization.expected_pvc_uid.clone(),
                                        },
                                    storage_generation:
                                        kuberic_runtime::protocol::types::StorageGeneration::new(format!(
                                            "storage-{}",
                                            initialization.expected_pvc_uid
                                        )),
                                },
                            progress: kuberic_runtime::protocol::public_operations::PublicProgress {
                                role:
                                    kuberic_runtime::protocol::public_operations::PublicProgressRole::Recovery,
                                first: 0,
                                last: 0,
                            },
                            service_location: None,
                            data_loss_recovery: None,
                            completed_operations: Vec::new(),
                            extensions: Default::default(),
                        })),
                    );
                }
                ProtocolCommand::PublicV1Operation(operation) => {
                    let key = ReplicaObservationKey::new(
                        operation.target.replica_id,
                        operation.target.instance_id.clone(),
                    );
                    let Some(RawAgentObservation::PublicV1Report(report)) =
                        observation.agents.get_mut(&key)
                    else {
                        return Err(ControllerError::ObservationStale);
                    };
                    let (call_kind, previous_configuration_id, current_configuration_id) =
                        in_memory_operation_metadata(&operation.intent)?;
                    let completion = PublicV1OperationCompletion {
                        operation_id: operation.intent.operation_id.clone(),
                        revision: operation.intent.revision,
                        call_kind,
                        process_session_id: operation.intent.process_session_id.clone(),
                        input_digest: operation.intent.input_digest.clone(),
                        replay_disposition: operation.intent.replay_disposition,
                        history: report.history.clone(),
                        storage_incarnation: report.storage_incarnation.clone(),
                        previous_configuration_id,
                        current_configuration_id,
                        catch_up_mode: None,
                        catch_up_specific_quorum: None,
                        result: PublicV1OperationResult::Succeeded,
                    };
                    if let Some(existing) = report
                        .completed_operations
                        .iter()
                        .find(|existing| existing.operation_id == completion.operation_id)
                    {
                        if existing != &completion {
                            return Err(ControllerError::InvalidAgentEvidence(
                                "in-memory operation completion changed".into(),
                            ));
                        }
                    } else {
                        report.completed_operations.push(completion);
                    }
                    if let Some(program) = operation.intent.program.as_ref() {
                        match program {
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Role {
                                epoch,
                                role,
                            } => {
                                report.role = *role;
                                report.progress.role = match role {
                                    ReplicaRole::Primary => kuberic_runtime::protocol::public_operations::PublicProgressRole::Primary,
                                    ReplicaRole::ActiveSecondary | ReplicaRole::IdleSecondary => kuberic_runtime::protocol::public_operations::PublicProgressRole::Secondary,
                                    ReplicaRole::None => kuberic_runtime::protocol::public_operations::PublicProgressRole::Recovery,
                                };
                                report.read_status =
                                    kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
                                report.write_status =
                                    kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
                                report.service_location = None;
                                let _ = epoch;
                            }
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Configuration(configuration) => {
                                if report.role == ReplicaRole::Primary {
                                    report.read_status = kuberic_runtime::protocol::types::AccessStatus::Granted;
                                    report.write_status = kuberic_runtime::protocol::types::AccessStatus::Granted;
                                    let role_completion = report
                                        .completed_operations
                                        .iter()
                                        .rev()
                                        .find(|completion| {
                                            completion.call_kind
                                                == kuberic_runtime::protocol::public_operations::PublicV1CallKind::Role
                                                && completion.result
                                                    == PublicV1OperationResult::Succeeded
                                        })
                                        .ok_or_else(|| {
                                            ControllerError::InvalidAgentEvidence(
                                                "primary configuration lacks completed role".into(),
                                            )
                                        })?;
                                    report.service_location = Some(
                                        kuberic_runtime::protocol::public_operations::ServiceLocation {
                                            identity: report.identity.clone(),
                                            resource_uid: report.resource_uid.clone(),
                                            operation_id: role_completion.operation_id.clone(),
                                            replica: report.replica.clone(),
                                            process_session_id: report.process_session_id.clone(),
                                            epoch: configuration.current.epoch,
                                            revision: role_completion.revision,
                                            address: "http://127.0.0.1:8080".into(),
                                        },
                                    );
                                } else {
                                    report.read_status = kuberic_runtime::protocol::types::AccessStatus::Granted;
                                    report.write_status = kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
                                }
                            }
                            _ => {}
                        }
                    }
                    report.report_sequence += 1;
                    report
                        .validate()
                        .map_err(|message| ControllerError::InvalidAgentEvidence(message.into()))?;
                }
                ProtocolCommand::PublicV1History(_) => {}
            }
            Ok(())
        }

        #[cfg(feature = "runtime-test-bridge")]
        fn in_memory_operation_metadata(
            intent: &kuberic_runtime::protocol::public_operations::PublicOperationIntent,
        ) -> Result<(
            kuberic_runtime::protocol::public_operations::PublicV1CallKind,
            Option<kuberic_runtime::protocol::types::ConfigurationId>,
            Option<kuberic_runtime::protocol::types::ConfigurationId>,
        )> {
            use kuberic_runtime::protocol::public_operations::{
                PublicOperationProgram, PublicV1CallKind,
            };
            match intent.program.as_ref() {
                Some(PublicOperationProgram::Open { .. }) => {
                    Ok((PublicV1CallKind::Open, None, None))
                }
                Some(PublicOperationProgram::Role { .. }) => {
                    Ok((PublicV1CallKind::Role, None, None))
                }
                Some(PublicOperationProgram::Epoch { .. }) => {
                    Ok((PublicV1CallKind::Epoch, None, None))
                }
                Some(PublicOperationProgram::Configuration(configuration)) => Ok((
                    PublicV1CallKind::Configuration,
                    configuration
                        .previous
                        .as_ref()
                        .map(|previous| previous.configuration_id.clone()),
                    Some(configuration.current.configuration_id.clone()),
                )),
                Some(PublicOperationProgram::CatchUp { configuration, .. }) => Ok((
                    PublicV1CallKind::CatchUp,
                    configuration
                        .previous
                        .as_ref()
                        .map(|previous| previous.configuration_id.clone()),
                    Some(configuration.current.configuration_id.clone()),
                )),
                Some(PublicOperationProgram::Progress { .. }) => {
                    Ok((PublicV1CallKind::Progress, None, None))
                }
                Some(PublicOperationProgram::Swap { .. }) => {
                    Ok((PublicV1CallKind::Swap, None, None))
                }
                Some(PublicOperationProgram::Build(build)) => Ok((
                    PublicV1CallKind::Build,
                    None,
                    build.current_configuration_id.clone(),
                )),
                Some(PublicOperationProgram::Remove(_)) => {
                    Ok((PublicV1CallKind::Remove, None, None))
                }
                Some(PublicOperationProgram::Close) => Ok((PublicV1CallKind::Close, None, None)),
                Some(PublicOperationProgram::Abort) => Ok((PublicV1CallKind::Abort, None, None)),
                None => Err(ControllerError::InvalidAgentEvidence(
                    "in-memory operation lacks program".into(),
                )),
            }
        }
        drop(state);
        if response_lost {
            return Err(ControllerError::AgentUnavailable(
                "ambiguous command result after dispatch".to_string(),
            ));
        }
        Ok(())
    }
}

#[allow(dead_code)]
async fn delete_exact<K>(api: &Api<K>, name: &str, uid: &str) -> Result<()>
where
    K: Clone + std::fmt::Debug + serde::de::DeserializeOwned + kube::Resource<DynamicType = ()>,
{
    match api
        .delete(
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
    {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(response)) if response.code == 404 => Ok(()),
        Err(error) => Err(map_kube_effect_error(error)),
    }
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

#[cfg(feature = "runtime-test-bridge")]
fn validate_in_memory_public_v1_scope(
    observation: &RawObservation,
    scope: &PublicV1Scope,
) -> std::result::Result<(), String> {
    if observation.set.namespace().as_deref() != Some(scope.namespace.as_str())
        || observation.set.name_any() != scope.name
        || observation.set.uid().as_deref() != Some(scope.resource_uid.as_str())
    {
        return Err("public-v1 scope differs from in-memory observation".into());
    }
    observation
        .set
        .status
        .as_ref()
        .map_or(Ok(()), KubericSetStatus::validate_public_v1)
}

#[cfg(feature = "runtime-test-bridge")]
fn advance_in_memory_resource_version(state: &mut InMemoryState) {
    let next = state
        .observation
        .set
        .resource_version()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or_default()
        + 1;
    state.observation.set.metadata.resource_version = Some(next.to_string());
}

#[async_trait]
#[cfg(feature = "runtime-test-bridge")]
impl PublicV1HistoryApi for InMemoryClusterApi {
    async fn observe_public_v1_status(
        &self,
        scope: &PublicV1Scope,
    ) -> std::result::Result<Option<PublicV1AcceptedStatus>, String> {
        let state = self.state.lock().await;
        validate_in_memory_public_v1_scope(&state.observation, scope)?;
        accepted_public_v1_status(state.observation.set.status.as_ref())
    }

    async fn persist_public_v1_status(
        &self,
        scope: &PublicV1Scope,
        expected: Option<&PublicV1AcceptedStatus>,
        next: &PublicV1AcceptedStatus,
    ) -> std::result::Result<(), String> {
        next.validate_evidence().map_err(str::to_owned)?;
        let mut state = self.state.lock().await;
        validate_in_memory_public_v1_scope(&state.observation, scope)?;
        let status = state
            .observation
            .set
            .status
            .get_or_insert_with(KubericSetStatus::default);
        if accepted_public_v1_status(Some(status))?.as_ref() != expected {
            return Err("stale in-memory public-v1 history".into());
        }
        status.public_v1_authority = Some(PublicV1AuthorityStatus::from_authority(next)?);
        if let Some(transition_state) = status.public_v1_transition.as_mut() {
            let mut transition = transition_state.transition()?;
            transition.evidence = next.clone();
            *transition_state = PublicV1TransitionState::from_transition(&transition)?;
        }
        advance_in_memory_resource_version(&mut state);
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
    }

    async fn dispatch_public_v1_history(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedStatus,
        command: &ProtocolCommand,
    ) -> std::result::Result<(), String> {
        let observation = self.observation().await;
        validate_in_memory_public_v1_scope(&observation, scope)?;
        if accepted_public_v1_status(observation.set.status.as_ref())?.as_ref() != Some(expected) {
            return Err("stale in-memory public-v1 history dispatch".into());
        }
        self.execute_command(&observation, command)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
#[cfg(feature = "runtime-test-bridge")]
impl PublicV1SessionRenewalApi for InMemoryClusterApi {
    async fn observe_public_v1_session_renewal(
        &self,
        scope: &PublicV1Scope,
    ) -> std::result::Result<Option<PublicV1SessionRenewalStatus>, String> {
        let state = self.state.lock().await;
        validate_in_memory_public_v1_scope(&state.observation, scope)?;
        accepted_public_v1_renewal(state.observation.set.status.as_ref())
    }

    async fn persist_public_v1_session_renewal(
        &self,
        scope: &PublicV1Scope,
        expected: Option<&PublicV1SessionRenewalStatus>,
        next: &PublicV1SessionRenewalStatus,
    ) -> std::result::Result<(), String> {
        next.validate().map_err(str::to_owned)?;
        let mut state = self.state.lock().await;
        validate_in_memory_public_v1_scope(&state.observation, scope)?;
        let status = state
            .observation
            .set
            .status
            .get_or_insert_with(KubericSetStatus::default);
        if accepted_public_v1_renewal(Some(status))?.as_ref() != expected {
            return Err("stale in-memory public-v1 session renewal".into());
        }
        status.public_v1_session_renewal = Some(PublicV1SessionRenewalState::from_renewal(next)?);
        advance_in_memory_resource_version(&mut state);
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
    }

    async fn dispatch_public_v1_session_renewal(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1SessionRenewalStatus,
        command: &ProtocolCommand,
    ) -> std::result::Result<(), String> {
        let observation = self.observation().await;
        validate_in_memory_public_v1_scope(&observation, scope)?;
        if accepted_public_v1_renewal(observation.set.status.as_ref())?.as_ref() != Some(expected) {
            return Err("stale in-memory public-v1 session-renewal dispatch".into());
        }
        self.execute_command(&observation, command)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait]
#[cfg(feature = "runtime-test-bridge")]
impl PublicV1OperationApi for InMemoryClusterApi {
    async fn observe_public_v1_topology(
        &self,
        _scope: &PublicV1Scope,
    ) -> std::result::Result<Option<ConfigurationDescriptor>, String> {
        self.state
            .lock()
            .await
            .observation
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .map(PublicV1TopologyState::configuration)
            .transpose()
    }

    async fn persist_public_v1_topology(
        &self,
        _scope: &PublicV1Scope,
        expected: Option<&kuberic_runtime::protocol::types::ConfigurationId>,
        next: &ConfigurationDescriptor,
    ) -> std::result::Result<(), String> {
        let mut state = self.state.lock().await;
        let status = state
            .observation
            .set
            .status
            .get_or_insert_with(KubericSetStatus::default);
        let current = status
            .public_v1_topology
            .as_ref()
            .map(PublicV1TopologyState::configuration)
            .transpose()?;
        if current
            .as_ref()
            .map(|configuration| &configuration.configuration_id)
            != expected
            || status.public_v1_transition.is_some()
            || status.public_v1_operation.is_some()
        {
            return Err("stale public-v1 topology persistence".into());
        }
        status.public_v1_topology = Some(PublicV1TopologyState::from_configuration(next)?);
        advance_in_memory_resource_version(&mut state);
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
    }

    async fn begin_public_v1_operation(
        &self,
        _scope: &PublicV1Scope,
        expected_authority: &PublicV1AcceptedStatus,
        transition: &PublicV1AcceptedTransitionStatus,
        command: &kuberic_runtime::protocol::command::PublicV1OperationCommand,
    ) -> std::result::Result<(), String> {
        transition.validate()?;
        command.validate().map_err(str::to_owned)?;
        if &transition.evidence != expected_authority
            || !public_v1_operations::operation_matches_transition(transition, &command.intent)
            || transition.intent.source.storage_incarnation.replica != command.target
        {
            return Err("public-v1 operation differs from proposed transition".into());
        }
        let mut state = self.state.lock().await;
        let status = state
            .observation
            .set
            .status
            .get_or_insert_with(KubericSetStatus::default);
        if accepted_public_v1_status(Some(status))?.as_ref() != Some(expected_authority)
            || status.public_v1_transition.is_some()
            || status.public_v1_operation.is_some()
        {
            return Err("stale public-v1 operation proposal".into());
        }
        status.public_v1_transition = Some(PublicV1TransitionState::from_transition(transition)?);
        status.public_v1_operation = Some(PublicV1OperationStatus::from_command(command)?);
        advance_in_memory_resource_version(&mut state);
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
    }

    async fn observe_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
    ) -> std::result::Result<Option<PublicV1AcceptedTransitionStatus>, String> {
        let state = self.state.lock().await;
        validate_in_memory_public_v1_scope(&state.observation, scope)?;
        normalized_transition(&state.observation)
    }

    async fn dispatch_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        command: &ProtocolCommand,
    ) -> std::result::Result<(), String> {
        let observation = self.observation().await;
        validate_in_memory_public_v1_scope(&observation, scope)?;
        if normalized_transition(&observation)?.as_ref() != Some(expected) {
            return Err("stale in-memory public-v1 operation".into());
        }
        self.execute_command(&observation, command)
            .await
            .map_err(|error| error.to_string())
    }

    async fn observe_public_v1_operation_report(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
    ) -> std::result::Result<PublicV1AgentReport, String> {
        let observation = self.observation().await;
        validate_in_memory_public_v1_scope(&observation, scope)?;
        let identity = &expected.intent.source.storage_incarnation.replica;
        let key = ReplicaObservationKey::new(identity.replica_id, identity.instance_id.clone());
        match observation.agents.get(&key) {
            Some(RawAgentObservation::PublicV1Report(report)) => Ok((**report).clone()),
            _ => Err("in-memory public-v1 report is unavailable".into()),
        }
    }
}

#[async_trait]
#[cfg(feature = "runtime-test-bridge")]
impl PublicV1EvidenceApi for InMemoryClusterApi {
    async fn observe_public_v1_evidence_status(
        &self,
        scope: &PublicV1Scope,
    ) -> std::result::Result<Option<PublicV1AcceptedTransitionStatus>, String> {
        self.observe_public_v1_operation(scope).await
    }

    async fn persist_public_v1_evidence_status(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        next: &PublicV1AcceptedStatus,
    ) -> std::result::Result<(), String> {
        let mut state = self.state.lock().await;
        validate_in_memory_public_v1_scope(&state.observation, scope)?;
        if normalized_transition(&state.observation)?.as_ref() != Some(expected) {
            return Err("stale in-memory public-v1 evidence".into());
        }
        let status = state
            .observation
            .set
            .status
            .as_mut()
            .ok_or_else(|| "public-v1 status disappeared".to_string())?;
        status.public_v1_authority = Some(PublicV1AuthorityStatus::from_authority(next)?);
        let transition_state = status
            .public_v1_transition
            .as_mut()
            .ok_or_else(|| "public-v1 transition disappeared".to_string())?;
        let mut transition = transition_state.transition()?;
        transition.evidence = next.clone();
        *transition_state = PublicV1TransitionState::from_transition(&transition)?;
        advance_in_memory_resource_version(&mut state);
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
    }

    async fn accept_public_v1_completion(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
    ) -> std::result::Result<(), String> {
        if !expected.intent.matches(completion) {
            return Err("in-memory public-v1 completion differs from intent".into());
        }
        clear_in_memory_public_v1_transition(self, scope, expected).await
    }

    async fn retire_public_v1_incarnation(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
        _target: &PublicV1ResourceAuthority,
    ) -> std::result::Result<(), String> {
        if !expected.intent.matches(completion) {
            return Err("in-memory public-v1 retirement differs from intent".into());
        }
        clear_in_memory_public_v1_transition(self, scope, expected).await
    }

    async fn provision_public_v1_replacement(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
        _target: &PublicV1ResourceAuthority,
        _rebuild: &CanonicalReplicaRebuildRequired,
    ) -> std::result::Result<(), String> {
        if !expected.intent.matches(completion) {
            return Err("in-memory public-v1 replacement differs from intent".into());
        }
        clear_in_memory_public_v1_transition(self, scope, expected).await
    }
}

#[cfg(feature = "runtime-test-bridge")]
async fn clear_in_memory_public_v1_transition(
    api: &InMemoryClusterApi,
    scope: &PublicV1Scope,
    expected: &PublicV1AcceptedTransitionStatus,
) -> std::result::Result<(), String> {
    let mut state = api.state.lock().await;
    validate_in_memory_public_v1_scope(&state.observation, scope)?;
    if normalized_transition(&state.observation)?.as_ref() != Some(expected) {
        return Err("stale in-memory public-v1 transition completion".into());
    }
    let status = state
        .observation
        .set
        .status
        .as_mut()
        .ok_or_else(|| "public-v1 status disappeared".to_string())?;
    status.public_v1_transition = None;
    status.public_v1_operation = None;
    advance_in_memory_resource_version(&mut state);
    state.effects.push(EffectRecord::ReplaceStatus);
    Ok(())
}

#[cfg(feature = "runtime-test-bridge")]
mod public_v1_service;
#[cfg(feature = "runtime-test-bridge")]
pub use public_v1_service::{
    PublicV1ServiceApi, public_v1_service_matches, public_v1_service_update,
};
mod public_v1_history;
pub use public_v1_history::{PublicV1HistoryApi, PublicV1Scope, PublicV1SessionRenewalApi};
pub(crate) mod public_v1_operations;
pub use public_v1_operations::PublicV1OperationApi;
#[cfg(feature = "runtime-test-bridge")]
pub use public_v1_operations::{HostedPublicV1Harness, HostedPublicV1Replica};
mod public_v1_evidence;
pub use public_v1_evidence::{
    PublicV1EvidenceApi, PublicV1EvidenceEffect, PublicV1EvidenceResource, PublicV1EvidenceState,
    PublicV1ResourceDisposition, PublicV1ResourceProof,
};
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_kubernetes_api_requires_one_exact_namespace() {
        for namespace in ["default", "kuberic-system", "ns1"] {
            assert!(KubeClusterApi::<GrpcAgentApi>::valid_namespace(namespace));
        }
        for namespace in ["", "-bad", "bad-", "Bad", "two.labels"] {
            assert!(!KubeClusterApi::<GrpcAgentApi>::valid_namespace(namespace));
        }
        assert_eq!(
            KubeClusterApi::<GrpcAgentApi>::namespace_mismatch("kuberic-system", "other")
                .unwrap_err(),
            ControllerError::Observation(
                "requested namespace differs from the controller installation namespace"
                    .to_string()
            )
        );

        let source = include_str!("cluster_api.rs");
        assert!(!source.contains(concat!("installation_namespace", ": Option")));
        assert!(!source.contains(concat!("pub fn ", "new_namespaced")));
        assert!(!source.contains(concat!(
            "is_none_or(|configured| configured",
            " == namespace)"
        )));
    }
}
