use std::collections::BTreeMap;
use std::net::IpAddr;
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
use kuberic_runtime::protocol::command::{
    InitializeAgentStore, ProtocolCommand, PublicV1OperationCommand,
};
use kuberic_runtime::protocol::observation::ReplicaObservationKey;
use kuberic_runtime::protocol::public_operations::{
    CanonicalReplicaRebuildRequired, PublicBuildAttemptReference, PublicConfiguration,
    PublicInboundBuildAuthority, PublicOperationClass, PublicOperationIntent,
    PublicOperationProgram, PublicReplicaDescriptor, PublicV1AcceptedStatus, PublicV1AgentReport,
    PublicV1OperationCompletion, PublicV1OperationResult, PublicV1SessionRenewalStatus,
};
use kuberic_runtime::protocol::types::{
    ConfigurationDescriptor, ConfigurationId, EffectivePolicy, Epoch, OperationId, PodUid,
    ProcessSessionEndpoint, ProvisioningIntent, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ReplicaStorageIncarnationContext, ResourceUid,
    derive_agent_generation, derive_initialization_id, derive_replacement_resource_name,
    derive_replica_endpoint_name,
};
#[cfg(feature = "runtime-test-bridge")]
use tokio::sync::Mutex;
use tonic::Code;

use crate::crd::{
    CONTROL_ADDRESS_ANNOTATION, CONTROLLER_NAME, INSTANCE_LABEL, KubericSet, KubericSetStatus,
    PublicV1AuthorityStatus, PublicV1OperationStatus, PublicV1ReplacementState,
    PublicV1SessionRenewalState, PublicV1TopologyState, PublicV1TransitionState, REPLICA_ID_LABEL,
    SCALE_UP_ALLOCATION_ANNOTATION, SET_UID_LABEL,
};
use crate::evaluator::public_v1_evidence::{
    PublicV1AcceptedTransitionStatus, PublicV1ResourceAuthority,
};
use crate::evaluator::public_v1_topology::PublicV1TopologyReplica;
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
    RoutePrimary(ReplicaIdentity),
    ReplaceStatus,
    Execute(ProtocolCommand),
}

async fn revoke_inbound_build_authority<A>(
    api: &KubeClusterApi<A>,
    observation: &RawObservation,
    expected: &PublicV1AcceptedTransitionStatus,
    command: &ProtocolCommand,
) -> std::result::Result<(), String>
where
    A: AgentApi + 'static,
{
    let ProtocolCommand::PublicV1Operation(operation) = command else {
        return Ok(());
    };
    let Some(PublicOperationProgram::Build(build)) = operation.intent.program.as_ref() else {
        return Ok(());
    };
    let authority = inbound_build_authority(expected, operation, build)?;
    api.execute_command(
        observation,
        &ProtocolCommand::PublicV1History(Box::new(
            kuberic_runtime::protocol::command::PublicV1HistoryCommand::
                RevokeInboundBuildAuthority {
                    authority: Box::new(authority),
                },
        )),
    )
    .await
    .map_err(|error| error.to_string())
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
        let now_unix_seconds = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| ControllerError::Observation(error.to_string()))?
            .as_secs() as i64;
        let mut raw = RawObservation {
            set,
            pods,
            pvcs,
            services,
            secrets,
            failures,
            agents: BTreeMap::new(),
            now_unix_seconds,
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
                if !should_observe_replica(&raw.set, raw.now_unix_seconds, replica_id) {
                    return None;
                }
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
        if let ProtocolCommand::PublicV1History(public_command) = command
            && let kuberic_runtime::protocol::command::PublicV1HistoryCommand::SessionRenewal {
                command: renewal,
            } = public_command.as_ref()
        {
            let expected = renewal.expected_endpoint();
            let report = observation
                .agents
                .values()
                .find_map(|agent| match agent {
                    RawAgentObservation::PublicV1Report(report)
                        if report.replica == expected.identity =>
                    {
                        Some(report.as_ref())
                    }
                    _ => None,
                })
                .ok_or(ControllerError::ObservationStale)?;
            if report.storage_incarnation != expected.storage_incarnation
                || report.process_session_id != expected.process_session_id
                || report.replicator_creation_id != expected.replicator_creation_id
            {
                return Err(ControllerError::InvalidAgentEvidence(
                    "session-renewal dispatch endpoint differs from accepted authority".into(),
                ));
            }
        }
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
        validate_execute_observation(command, report)
    }
}

fn unavailable_primary_after_delay(set: &KubericSet, now_unix_seconds: i64) -> Option<ReplicaId> {
    let topology = set.status.as_ref()?.public_v1_topology.as_ref()?;
    let primary = topology.unavailable_primary_id?;
    let since = topology.unavailable_since_unix_seconds?;
    let delay = i64::try_from(set.spec.failover_delay_seconds).unwrap_or(i64::MAX);
    (now_unix_seconds.saturating_sub(since) >= delay).then_some(primary)
}

fn should_observe_replica(set: &KubericSet, now_unix_seconds: i64, replica_id: ReplicaId) -> bool {
    unavailable_primary_after_delay(set, now_unix_seconds) != Some(replica_id)
}

fn validate_execute_observation(
    command: &ProtocolCommand,
    report: proto::AgentStatusReport,
) -> Result<()> {
    if let ProtocolCommand::InitializeAgentStore(initialization) = command {
        let report = kuberic_runtime::control::normalize_uninitialized_agent_status_report(report)
            .map_err(|error| ControllerError::InvalidAgentEvidence(error.to_string()))?;
        if report.resource_uid != initialization.resource_uid
            || report.replica_id != initialization.local_replica_id
            || report.pod_uid != initialization.expected_pod_uid
            || report.pvc_uid != initialization.expected_pvc_uid
        {
            return Err(ControllerError::InvalidAgentEvidence(
                "InitializeAgentStore response differs from the accepted target".to_string(),
            ));
        }
        return Ok(());
    }
    kuberic_runtime::control::normalize_public_v1_agent_status_report(report)
        .map(|_| ())
        .map_err(|error| ControllerError::InvalidAgentEvidence(error.to_string()))
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
        if !observation.failures.is_empty() {
            return Err(observation
                .failures
                .iter()
                .map(|failure| format!("{}: {}", failure.source, failure.message))
                .collect::<Vec<_>>()
                .join("; "));
        }
        validate_public_v1_controller_status(observation.set.status.as_ref())?;
        Ok(observation)
    }

    async fn observe_public_v1_status_scope(
        &self,
        scope: &PublicV1Scope,
    ) -> std::result::Result<RawObservation, String> {
        self.require_installation_namespace(&scope.namespace)
            .map_err(|error| error.to_string())?;
        let sets: Api<KubericSet> = Api::namespaced(self.client.clone(), &scope.namespace);
        let set = sets
            .get(&scope.name)
            .await
            .map_err(|error| error.to_string())?;
        if set.uid().as_deref() != Some(scope.resource_uid.as_str()) {
            return Err("public-v1 scope no longer identifies the observed resource".into());
        }
        validate_public_v1_controller_status(set.status.as_ref())?;
        Ok(RawObservation {
            set,
            pods: Vec::new(),
            pvcs: Vec::new(),
            services: Vec::new(),
            secrets: Vec::new(),
            agents: BTreeMap::new(),
            failures: Vec::new(),
            now_unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| error.to_string())?
                .as_secs() as i64,
        })
    }

    async fn observe_public_v1_target_scope(
        &self,
        scope: &PublicV1Scope,
        identity: &ReplicaIdentity,
    ) -> std::result::Result<RawObservation, String> {
        let mut observation = self.observe_public_v1_status_scope(scope).await?;
        let pods: Api<Pod> = Api::namespaced(self.client.clone(), &scope.namespace);
        let selector = format!(
            "{SET_UID_LABEL}={},{}={}",
            scope.resource_uid, REPLICA_ID_LABEL, identity.replica_id
        );
        let pod = pods
            .list(&ListParams::default().labels(&selector))
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .find(|pod| pod.uid().as_deref() == Some(identity.instance_id.as_str()))
            .ok_or_else(|| "exact public-v1 operation target Pod is unavailable".to_string())?;
        let key = ReplicaObservationKey::new(identity.replica_id, identity.instance_id.clone());
        let report = self
            .observe_agent(&pod, &scope.resource_uid, identity.replica_id)
            .await;
        observation.pods.push(pod);
        observation.agents.insert(key, report);
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
        let observation = self.observe_public_v1_status_scope(scope).await?;
        if !normalized_transition(&observation)?.is_some_and(|current| {
            current.evidence == expected.evidence && current.intent == expected.intent
        }) {
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

enum CorrectiveReplacementAction {
    Persist {
        next: PublicV1ReplacementState,
        clear_transition: bool,
    },
    Cleanup {
        next: PublicV1ReplacementState,
    },
}

fn replacement_target_is_admitted(
    status: &KubericSetStatus,
    target: &ReplicaStorageIncarnationContext,
) -> bool {
    status.public_v1_topology.as_ref().is_some_and(|topology| {
        topology.sessions.iter().any(|session| {
            session.identity == target.replica && session.storage_incarnation == *target
        })
    })
}

fn corrective_replacement_target_pod_present(
    observation: &RawObservation,
    target: &ReplicaStorageIncarnationContext,
) -> bool {
    observation
        .pods
        .iter()
        .any(|pod| pod.uid().as_deref() == Some(target.replica.instance_id.as_str()))
}

pub(crate) fn replica_storage_incarnation_remains(
    observation: &RawObservation,
    target: &ReplicaStorageIncarnationContext,
) -> bool {
    let exact_agent_remains = observation.agents.iter().any(|(key, agent)| match agent {
        RawAgentObservation::Absent => false,
        RawAgentObservation::Unavailable { .. } | RawAgentObservation::Invalid { .. } => {
            key.replica_id == target.replica.replica_id
                && key.instance_id == target.replica.instance_id
        }
        RawAgentObservation::Uninitialized(agent) => {
            agent.replica_id == target.replica.replica_id
                && agent.pod_uid.as_str() == target.replica.instance_id.as_str()
                && match &target.storage {
                    kuberic_runtime::protocol::types::ReplicaStorageIdentity::Persistent {
                        pod_uid,
                        pvc_uid,
                    } => &agent.pod_uid == pod_uid && &agent.pvc_uid == pvc_uid,
                    kuberic_runtime::protocol::types::ReplicaStorageIdentity::Volatile {
                        ..
                    } => false,
                }
        }
        RawAgentObservation::PublicV1Report(report) => report.storage_incarnation == *target,
    });
    if exact_agent_remains {
        return true;
    }
    let kuberic_runtime::protocol::types::ReplicaStorageIdentity::Persistent { pod_uid, pvc_uid } =
        &target.storage
    else {
        return false;
    };
    observation
        .pods
        .iter()
        .any(|pod| pod.uid().as_deref() == Some(pod_uid.as_str()))
        || observation
            .pvcs
            .iter()
            .any(|pvc| pvc.uid().as_deref() == Some(pvc_uid.as_str()))
        || observation.services.iter().any(|service| {
            service.spec.as_ref().is_some_and(|spec| {
                spec.selector.as_ref().is_some_and(|selector| {
                    selector.get(INSTANCE_LABEL).map(String::as_str) == Some(pod_uid.as_str())
                })
            })
        })
}

fn plan_corrective_replacement(
    existing: Option<&PublicV1ReplacementState>,
    configuration_id: &ConfigurationId,
    target: &ReplicaStorageIncarnationContext,
    target_was_admitted: bool,
) -> std::result::Result<CorrectiveReplacementAction, String> {
    match existing {
        None => Ok(CorrectiveReplacementAction::Persist {
            next: PublicV1ReplacementState::new(configuration_id, target.clone())?,
            clear_transition: true,
        }),
        Some(existing) if existing.predecessor == *target => {
            Ok(CorrectiveReplacementAction::Persist {
                next: existing.clone(),
                clear_transition: true,
            })
        }
        Some(existing) if existing.replacement.as_ref() == Some(target) => {
            Ok(CorrectiveReplacementAction::Persist {
                next: existing.begin_replacement_retirement(target)?,
                clear_transition: false,
            })
        }
        Some(existing) if existing.retiring_replacement.as_ref() == Some(target) => {
            Ok(CorrectiveReplacementAction::Cleanup {
                next: existing.complete_replacement_retirement(
                    configuration_id,
                    target,
                    target_was_admitted,
                )?,
            })
        }
        Some(_) => Err("corrective replacement differs from active authority".into()),
    }
}

fn topology_sessions(
    observation: &RawObservation,
    configuration: &ConfigurationDescriptor,
) -> std::result::Result<Vec<ProcessSessionEndpoint>, String> {
    configuration
        .members
        .iter()
        .map(|member| {
            let report = observation
                .agents
                .values()
                .find_map(|agent| match agent {
                    RawAgentObservation::PublicV1Report(report)
                        if report.replica == member.identity && report.role == member.role =>
                    {
                        Some(report.as_ref())
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    format!(
                        "public-v1 topology member {} lacks its exact admitted session",
                        member.identity.replica_id
                    )
                })?;
            Ok(ProcessSessionEndpoint {
                identity: report.replica.clone(),
                storage_incarnation: report.storage_incarnation.clone(),
                process_session_id: report.process_session_id.clone(),
                replicator_creation_id: report.replicator_creation_id.clone(),
                role: member.role,
            })
        })
        .collect()
}

fn renewed_topology_sessions(
    observation: &RawObservation,
    renewal: &PublicV1SessionRenewalStatus,
) -> std::result::Result<Vec<ProcessSessionEndpoint>, String> {
    renewal
        .required
        .fresh_configuration
        .members
        .iter()
        .map(|member| {
            let expected = renewal
                .required
                .replacement
                .iter()
                .chain(renewal.required.surviving_sessions.iter())
                .find(|endpoint| endpoint.identity == member.identity)
                .ok_or_else(|| {
                    "session-renewal configuration lacks an accepted endpoint".to_string()
                })?;
            let report = observation
                .agents
                .values()
                .find_map(|agent| match agent {
                    RawAgentObservation::PublicV1Report(report)
                        if report.replica == expected.identity =>
                    {
                        Some(report.as_ref())
                    }
                    _ => None,
                })
                .ok_or_else(|| "session-renewal completion lacks its exact report".to_string())?;
            if report.storage_incarnation != expected.storage_incarnation
                || report.process_session_id != expected.process_session_id
                || report.replicator_creation_id != expected.replicator_creation_id
                || report.role != member.role
            {
                return Err(
                    "session-renewal completion endpoint differs from accepted authority".into(),
                );
            }
            Ok(ProcessSessionEndpoint {
                identity: report.replica.clone(),
                storage_incarnation: report.storage_incarnation.clone(),
                process_session_id: report.process_session_id.clone(),
                replicator_creation_id: report.replicator_creation_id.clone(),
                role: report.role,
            })
        })
        .collect()
}

fn topology_state_with_switchover_completion(
    set: &KubericSet,
    status: &KubericSetStatus,
    configuration: &ConfigurationDescriptor,
    sessions: &[ProcessSessionEndpoint],
) -> std::result::Result<PublicV1TopologyState, String> {
    let mut topology = PublicV1TopologyState::from_configuration(configuration, sessions)?;
    let previous = status.public_v1_topology.as_ref();
    topology.completed_switchover_request_id =
        previous.and_then(|previous| previous.completed_switchover_request_id.clone());
    if let Some(previous) = previous
        && let Some(request) = &previous.pending_switchover
    {
        let starting = previous.configuration()?;
        let target = ReplicaId::new(i64::from(request.target_replica_id));
        let finishing = ConfigurationDescriptor::new(
            Epoch::new(
                starting.epoch.data_loss_number,
                starting.epoch.configuration_number + 1,
            ),
            target,
            starting
                .members
                .iter()
                .cloned()
                .map(|mut member| {
                    member.role = if member.identity.replica_id == target {
                        ReplicaRole::Primary
                    } else {
                        ReplicaRole::ActiveSecondary
                    };
                    member
                })
                .collect(),
            starting.write_quorum,
        );
        if configuration == &starting && sessions == previous.sessions {
            topology.pending_switchover = Some(request.clone());
        } else if configuration == &finishing
            && sessions.iter().all(|session| {
                previous.sessions.iter().any(|old| {
                    let mut expected = old.clone();
                    expected.role = session.role;
                    expected == *session
                })
            })
        {
            topology.completed_switchover_request_id = Some(request.request_id.clone());
        } else {
            return Err("pending switchover cannot publish unrelated topology or sessions".into());
        }
    } else if let Some(request) = set.spec.switchover.as_ref().filter(|request| {
        topology.completed_switchover_request_id.as_deref() != Some(request.request_id.as_str())
    }) {
        if i64::from(request.target_replica_id) == configuration.primary_id.value() {
            topology.completed_switchover_request_id = Some(request.request_id.clone());
        } else if previous.is_some_and(|previous| {
            previous.configuration_id == configuration.configuration_id
                && previous.sessions == sessions
        }) {
            topology.pending_switchover = Some(request.clone());
            topology.configuration()?;
        }
    }
    Ok(topology)
}

fn renewal_topology_replicas(
    observation: &RawObservation,
    renewal: &PublicV1SessionRenewalStatus,
) -> std::result::Result<Vec<PublicV1TopologyReplica>, String> {
    renewal
        .required
        .fresh_configuration
        .members
        .iter()
        .map(|member| {
            let endpoint = renewal
                .required
                .replacement
                .iter()
                .chain(renewal.required.surviving_sessions.iter())
                .find(|endpoint| endpoint.identity == member.identity)
                .ok_or_else(|| {
                    "session-renewal configuration lacks an accepted endpoint".to_string()
                })?;
            let report = observation
                .agents
                .values()
                .find_map(|agent| match agent {
                    RawAgentObservation::PublicV1Report(report)
                        if report.replica == endpoint.identity
                            && report.storage_incarnation == endpoint.storage_incarnation
                            && report.process_session_id == endpoint.process_session_id
                            && report.replicator_creation_id == endpoint.replicator_creation_id =>
                    {
                        Some(report.as_ref())
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    "session-renewal configuration lacks its exact agent report".to_string()
                })?;
            observation
                .services
                .iter()
                .find(|service| {
                    service.spec.as_ref().is_some_and(|spec| {
                        spec.selector.as_ref().is_some_and(|selector| {
                            selector
                                .get(INSTANCE_LABEL)
                                .is_some_and(|value| value == member.identity.instance_id.as_str())
                        }) && spec.ports.as_ref().is_some_and(|ports| {
                            ports
                                .iter()
                                .any(|port| port.name.as_deref() == Some("replication"))
                        })
                    })
                })
                .ok_or_else(|| {
                    "session-renewal configuration lacks a replication Service".to_string()
                })?;
            let pod = observation
                .pods
                .iter()
                .find(|pod| pod.uid().as_deref() == Some(member.identity.instance_id.as_str()))
                .ok_or_else(|| {
                    "session-renewal configuration lacks its exact replica Pod".to_string()
                })?;
            pod.status
                .as_ref()
                .and_then(|status| status.pod_ip.as_deref())
                .ok_or_else(|| {
                    "session-renewal configuration lacks a replica Pod address".to_string()
                })?
                .parse::<IpAddr>()
                .map_err(|_| {
                    "session-renewal configuration has an invalid replica Pod address".to_string()
                })?;
            Ok(PublicV1TopologyReplica {
                report: report.clone(),
                replication_address: report.replication_address.clone().ok_or_else(|| {
                    "session-renewal configuration lacks its current Replicator Open endpoint"
                        .to_string()
                })?,
            })
        })
        .collect()
}

fn renewal_public_configuration(
    local: &ReplicaIdentity,
    replicas: &[PublicV1TopologyReplica],
    configuration: &ConfigurationDescriptor,
) -> std::result::Result<PublicConfiguration, String> {
    let peers = replicas
        .iter()
        .filter(|replica| replica.report.replica != *local)
        .map(|replica| {
            Ok(PublicReplicaDescriptor {
                build_id: OperationId::default(),
                identity: replica.report.replica.clone(),
                replication_address: replica.replication_address.clone(),
                process_session_id: replica.report.process_session_id.clone(),
                storage_incarnation: Some(replica.report.storage_incarnation.clone()),
                role: configuration
                    .members
                    .iter()
                    .find(|member| member.identity == replica.report.replica)
                    .map(|member| member.role)
                    .expect("renewal replica is configured"),
                current_progress: replica
                    .report
                    .progress
                    .current()
                    .ok_or("native current progress unavailable")?,
                catch_up_capability: replica
                    .report
                    .progress
                    .capability()
                    .ok_or("native catch-up capability unavailable")?,
                must_catchup: false,
                is_up: replica.report.has_current_open_endpoint(),
                is_ready: replica.report.has_current_open_endpoint()
                    && replica.report.progress.current().is_some()
                    && replica.report.progress.capability().is_some(),
            })
        })
        .collect::<std::result::Result<Vec<_>, String>>()?;
    let configuration = PublicConfiguration {
        current: configuration.clone(),
        previous: None,
        replicas: peers,
    };
    configuration.validate().map_err(str::to_owned)?;
    Ok(configuration)
}

fn renewal_configuration_commands(
    authority: &PublicV1AcceptedStatus,
    renewal: &PublicV1SessionRenewalStatus,
    replicas: &[PublicV1TopologyReplica],
) -> std::result::Result<Vec<ProtocolCommand>, String> {
    let configuration = &renewal.required.fresh_configuration;
    let mut members = configuration.members.iter().collect::<Vec<_>>();
    members.sort_by_key(|member| member.role == ReplicaRole::Primary);
    let mut commands = Vec::with_capacity(members.len());
    for member in members {
        let replica = replicas
            .iter()
            .find(|replica| replica.report.replica == member.identity)
            .ok_or_else(|| {
                "session-renewal configuration lacks a local replica report".to_string()
            })?;
        let next_revision = replica
            .report
            .completed_operations
            .iter()
            .map(|completion| completion.revision)
            .chain(
                authority
                    .completed_operations
                    .iter()
                    .filter(|completion| completion.storage_incarnation.replica == member.identity)
                    .map(|completion| completion.revision),
            )
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| "session-renewal configuration revision overflow".to_string())?;
        let program = PublicOperationProgram::Configuration(renewal_public_configuration(
            &member.identity,
            replicas,
            configuration,
        )?);
        let operation_id = OperationId::new(format!(
            "{}-configuration-{}",
            renewal.required.operation_id, member.identity.replica_id
        ));
        let digest = program.digest();
        let revision = if let Some(completion) =
            replica
                .report
                .completed_operations
                .iter()
                .find(|completion| {
                    completion.operation_id == operation_id
                        && completion.process_session_id == replica.report.process_session_id
                        && completion.input_digest == digest
                        && completion.replay_disposition == program.replay_disposition()
                        && completion.history == authority.history
                        && completion.storage_incarnation == replica.report.storage_incarnation
                        && completion.result == PublicV1OperationResult::Succeeded
                }) {
            completion.revision
        } else {
            next_revision
        };
        let intent = PublicOperationIntent {
            identity: authority.identity.clone(),
            operation_id,
            revision,
            process_session_id: replica.report.process_session_id.clone(),
            class: PublicOperationClass::Authority,
            input_digest: digest,
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        };
        let command = PublicV1OperationCommand {
            target: member.identity.clone(),
            intent,
        };
        command.validate().map_err(str::to_owned)?;
        commands.push(ProtocolCommand::PublicV1Operation(Box::new(command)));
    }

    Ok(commands)
}

fn renewal_completed_copy_revoke(
    renewal: &PublicV1SessionRenewalStatus,
    replicas: &[PublicV1TopologyReplica],
) -> std::result::Result<Option<ProtocolCommand>, String> {
    use kuberic_runtime::protocol::public_operations::PublicV1CallKind;
    let Some(replacement) = renewal
        .required
        .replacement
        .as_ref()
        .filter(|_| renewal.required.predecessor.role == ReplicaRole::ActiveSecondary)
    else {
        return Ok(None);
    };
    let source = replicas
        .iter()
        .find(|replica| {
            replica.report.replica.replica_id == renewal.required.fresh_configuration.primary_id
        })
        .ok_or("renewal revoke lacks exact source")?;
    let copy_id = OperationId::new(format!(
        "{}-native-copy-{}",
        renewal.required.operation_id, replacement.process_session_id
    ));
    let Some(completion) = source
        .report
        .completed_operations
        .iter()
        .find(|completion| {
            completion.operation_id == copy_id
                && completion.call_kind == PublicV1CallKind::Build
                && completion.result == PublicV1OperationResult::Succeeded
                && completion.process_session_id == source.report.process_session_id
                && completion.storage_incarnation == source.report.storage_incarnation
                && completion.history == renewal.required.history
        })
    else {
        return Ok(None);
    };
    let remote = source
        .report
        .remote_builds
        .iter()
        .find(|remote| remote.operation_id == copy_id && remote.revision == completion.revision)
        .ok_or("completed renewal copy lacks exact retained Build input for revocation")?;
    let build = &remote.build;
    let target = replicas
        .iter()
        .find(|replica| replica.report.replica == replacement.identity)
        .ok_or("renewal revoke lacks exact replacement")?;
    if build.attempt != copy_id
        || build.epoch != renewal.required.renewal_epoch
        || build.replica != replacement.identity
        || build.target_storage != replacement.storage_incarnation
        || build.process_session_id != replacement.process_session_id
        || target.report.process_session_id != replacement.process_session_id
        || target.report.storage_incarnation != replacement.storage_incarnation
        || build.source_address != source.replication_address
        || build.replication_address != target.replication_address
        || build.history.as_ref() != Some(&renewal.required.history)
        || completion.input_digest != PublicOperationProgram::Build(build.clone()).digest()
    {
        return Err("completed renewal copy revocation differs from frozen authority".into());
    }
    let authority = PublicInboundBuildAuthority {
        operation_id: copy_id.clone(),
        operation_revision: remote.revision,
        attempt: PublicBuildAttemptReference {
            resource_uid: renewal.required.resource_uid.clone(),
            epoch: build.epoch,
            build_id: copy_id,
            source: source.report.replica.clone(),
            source_storage: source.report.storage_incarnation.clone(),
            source_session: source.report.process_session_id.clone(),
            target: build.replica.clone(),
            target_storage: build.target_storage.clone(),
            target_session: build.process_session_id.clone(),
        },
        history: renewal.required.history.clone(),
        current_configuration_id: build
            .current_configuration_id
            .clone()
            .ok_or("renewal revoke lacks configuration")?,
        source_address: build.source_address.clone(),
        target_address: build.replication_address.clone(),
    };
    authority.validate().map_err(str::to_owned)?;
    // Historical success may justify only an exact compare-revoke, never
    // reinstall authority or fabricate a fresh engine observation.
    Ok(Some(ProtocolCommand::PublicV1History(Box::new(
        kuberic_runtime::protocol::command::PublicV1HistoryCommand::RevokeInboundBuildAuthority {
            authority: Box::new(authority),
        },
    ))))
}

fn renewal_native_command(
    authority: &PublicV1AcceptedStatus,
    renewal: &PublicV1SessionRenewalStatus,
    replicas: &[PublicV1TopologyReplica],
) -> std::result::Result<Option<ProtocolCommand>, String> {
    use crate::evaluator::public_v1_topology::{native_cold_copy_step, native_configuration_step};
    use kuberic_runtime::protocol::public_operations::{ProgressQueryResult, PublicV1CallKind};
    let Some(replacement) = renewal
        .required
        .replacement
        .as_ref()
        .filter(|_| renewal.required.predecessor.role == ReplicaRole::ActiveSecondary)
    else {
        return Ok(None);
    };
    let target = replicas
        .iter()
        .find(|replica| replica.report.replica == replacement.identity)
        .ok_or("native renewal lacks its replacement report")?;
    let source = replicas
        .iter()
        .find(|replica| {
            replica.report.replica.replica_id == renewal.required.fresh_configuration.primary_id
        })
        .ok_or("native renewal lacks its primary report")?;
    let copy_id = OperationId::new(format!(
        "{}-native-copy-{}",
        renewal.required.operation_id, replacement.process_session_id,
    ));
    if let Some(intent) = source.report.active_operations.iter().find(|intent| {
        intent.operation_id == copy_id
            && matches!(intent.program, Some(PublicOperationProgram::Build(_)))
    }) {
        return Ok(Some(ProtocolCommand::PublicV1Operation(Box::new(
            PublicV1OperationCommand {
                target: source.report.replica.clone(),
                intent: intent.clone(),
            },
        ))));
    }
    let copied = source.report.completed_operations.iter().any(|completion| {
        completion.operation_id == copy_id
            && completion.process_session_id == source.report.process_session_id
            && completion.storage_incarnation == source.report.storage_incarnation
            && completion.history == renewal.required.history
            && completion.call_kind == PublicV1CallKind::Build
            && completion.result == PublicV1OperationResult::Succeeded
    });
    let cold = matches!(
        target.report.progress.current_progress,
        ProgressQueryResult::NotQueried(_)
    );
    let next = if copied {
        native_configuration_step(replicas, &renewal.required.fresh_configuration)?
    } else if cold {
        native_cold_copy_step(replicas, source, target, renewal.required.renewal_epoch)?
    } else {
        return Ok(None);
    };
    let (replica, program, id) = if let Some((identity, program)) = next {
        let replica = replicas
            .iter()
            .find(|replica| replica.report.replica == identity)
            .ok_or("native renewal lost its exact participant")?;
        let id = crate::evaluator::public_v1_topology::content_addressed_operation_id(
            format!(
                "{}-native-{}",
                renewal.required.operation_id, identity.replica_id
            ),
            &program,
        );
        (replica, program, id)
    } else if cold && !copied {
        let base = source
            .report
            .configuration
            .as_ref()
            .ok_or("native copy requires an installed primary configuration")?;
        (
            source,
            PublicOperationProgram::Build(
                kuberic_runtime::protocol::public_operations::PublicBuildInput {
                    attempt: copy_id.clone(),
                    source_address: source.replication_address.clone(),
                    epoch: base.voting.current.epoch,
                    current_configuration_id: Some(base.voting.current.configuration_id.clone()),
                    replica: target.report.replica.clone(),
                    target_storage: target.report.storage_incarnation.clone(),
                    process_session_id: target.report.process_session_id.clone(),
                    replication_address: target.replication_address.clone(),
                    history: Some(renewal.required.history.clone()),
                },
            ),
            copy_id,
        )
    } else {
        return Ok(None);
    };
    if let Some(pending) = replica
        .report
        .retry_pending_operations
        .iter()
        .find(|pending| pending.intent.operation_id == id)
    {
        if pending.intent.identity != authority.identity
            || replica.report.history != authority.history
            || pending.intent.process_session_id != replica.report.process_session_id
            || pending.intent.program.as_ref() != Some(&program)
            || pending.intent.input_digest != program.digest()
        {
            return Err("native renewal retry differs from exact frozen operation".into());
        }
        return Ok(Some(ProtocolCommand::PublicV1Operation(Box::new(
            PublicV1OperationCommand {
                target: replica.report.replica.clone(),
                intent: pending.intent.clone(),
            },
        ))));
    }
    let retained = replica
        .report
        .completed_operations
        .iter()
        .filter(|completion| completion.process_session_id == replica.report.process_session_id)
        .map(|completion| completion.revision)
        .chain(
            replica
                .report
                .active_operations
                .iter()
                .map(|intent| intent.revision),
        )
        .chain(
            replica
                .report
                .remote_builds
                .iter()
                .map(|build| build.revision),
        )
        .chain(
            replica
                .report
                .retry_pending_operations
                .iter()
                .map(|pending| pending.intent.revision),
        )
        .chain(
            authority
                .completed_operations
                .iter()
                .filter(|completion| {
                    completion.process_session_id == replica.report.process_session_id
                })
                .map(|completion| completion.revision),
        )
        .max()
        .unwrap_or(0);
    let class = match &program {
        PublicOperationProgram::Build(build) => {
            kuberic_runtime::protocol::public_operations::PublicOperationClass::Build {
                target: build.replica.replica_id,
            }
        }
        PublicOperationProgram::Remove(build) => {
            kuberic_runtime::protocol::public_operations::PublicOperationClass::Remove {
                target: build.replica.replica_id,
            }
        }
        _ => kuberic_runtime::protocol::public_operations::PublicOperationClass::Authority,
    };
    let revision = match &program {
        PublicOperationProgram::Build(_) => retained,
        PublicOperationProgram::Remove(build) => {
            crate::evaluator::public_v1_topology::exact_remove_revision(
                authority, replica, &id, build,
            )?
        }
        _ => retained
            .checked_add(1)
            .ok_or("native renewal revision exhausted")?,
    };
    let intent = kuberic_runtime::protocol::public_operations::PublicOperationIntent {
        identity: authority.identity.clone(),
        operation_id: id,
        revision,
        process_session_id: replica.report.process_session_id.clone(),
        class,
        input_digest: program.digest(),
        replay_disposition: program.replay_disposition(),
        lifecycle: None,
        program: Some(program),
    };
    intent.validate().map_err(str::to_owned)?;
    Ok(Some(ProtocolCommand::PublicV1Operation(Box::new(
        PublicV1OperationCommand {
            target: replica.report.replica.clone(),
            intent,
        },
    ))))
}

/// Exposes the production renewal preparation planner for cross-crate contract tests.
#[cfg(feature = "runtime-test-bridge")]
pub fn plan_public_v1_renewal_preparation(
    authority: &PublicV1AcceptedStatus,
    renewal: &PublicV1SessionRenewalStatus,
    replicas: &[PublicV1TopologyReplica],
) -> std::result::Result<Option<ProtocolCommand>, String> {
    renewal_native_command(authority, renewal, replicas)
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
        source_address: build.source_address.clone(),
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
        let observation = self.observe_public_v1_status_scope(scope).await?;
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
        let observation = self.observe_public_v1_status_scope(scope).await?;
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

    async fn complete_public_v1_session_renewal(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1SessionRenewalStatus,
    ) -> std::result::Result<(), String> {
        if expected.stage
            != kuberic_runtime::protocol::public_operations::PublicSessionRenewalStage::Authorized
        {
            return Err("public-v1 session renewal is not authorized".into());
        }
        let observation = self.observe_public_v1_scope(scope).await?;
        let mut status = observation.set.status.clone().unwrap_or_default();
        if accepted_public_v1_renewal(Some(&status))?.as_ref() != Some(expected)
            || status.public_v1_transition.is_some()
            || status.public_v1_operation.is_some()
        {
            return Err("stale public-v1 session-renewal completion".into());
        }
        let current = status
            .public_v1_topology
            .as_ref()
            .ok_or_else(|| "session renewal lacks public-v1 topology".to_string())?
            .configuration()?;
        if current.epoch != expected.required.previous_epoch {
            return Err("session renewal changed its predecessor topology".into());
        }
        let replicas = renewal_topology_replicas(&observation, expected)?;
        let primary = replicas
            .iter()
            .find(|replica| {
                replica.report.replica.replica_id
                    == expected.required.fresh_configuration.primary_id
            })
            .ok_or_else(|| "session renewal lacks its exact primary report".to_string())?;
        if replicas.iter().any(|replica| {
            expected
                .required
                .fresh_configuration
                .members
                .iter()
                .any(|member| {
                    member.identity == replica.report.replica
                        && (replica.report.role != member.role
                            || replica.report.progress.current().is_none()
                            || replica.report.progress.current()
                                != primary.report.progress.current())
                })
        }) {
            let authority = accepted_public_v1_status(Some(&status))?
                .ok_or_else(|| "session renewal lacks accepted public-v1 authority".to_string())?;
            let revision = primary
                .report
                .completed_operations
                .iter()
                .map(|completion| completion.revision)
                .chain(
                    authority
                        .completed_operations
                        .iter()
                        .filter(|completion| {
                            completion.storage_incarnation.replica == primary.report.replica
                        })
                        .map(|completion| completion.revision),
                )
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| "session-renewal settlement revision overflow".to_string())?;
            let program = PublicOperationProgram::CatchUp {
                configuration: renewal_public_configuration(
                    &primary.report.replica,
                    &replicas,
                    &expected.required.fresh_configuration,
                )?,
                mode: kuberic_runtime::protocol::public_operations::PublicCatchUpMode::All,
            };
            let intent = PublicOperationIntent {
                identity: authority.identity,
                operation_id: crate::public_v1_reconciliation::renewal_settlement_id(
                    &expected.required.operation_id,
                    primary.report.replica.replica_id,
                    revision,
                    &program,
                ),
                revision,
                process_session_id: primary.report.process_session_id.clone(),
                class: PublicOperationClass::Authority,
                input_digest: program.digest(),
                replay_disposition: program.replay_disposition(),
                lifecycle: None,
                program: Some(program),
            };
            self.execute_command(
                &observation,
                &ProtocolCommand::PublicV1Operation(Box::new(PublicV1OperationCommand {
                    target: primary.report.replica.clone(),
                    intent,
                })),
            )
            .await
            .map_err(|error| error.to_string())?;
            return Ok(());
        }
        let sessions = renewed_topology_sessions(&observation, expected)?;
        status.public_v1_topology = Some(topology_state_with_switchover_completion(
            &observation.set,
            &status,
            &expected.required.fresh_configuration,
            &sessions,
        )?);
        status.public_v1_session_renewal = None;
        self.replace_public_v1_status(&observation, status).await
    }

    async fn dispatch_public_v1_session_renewal(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1SessionRenewalStatus,
        command: &ProtocolCommand,
    ) -> std::result::Result<(), String> {
        let mut observation = self.observe_public_v1_scope(scope).await?;
        if accepted_public_v1_renewal(observation.set.status.as_ref())?.as_ref() != Some(expected) {
            return Err("stale public-v1 session-renewal dispatch".into());
        }
        let ProtocolCommand::PublicV1History(public_command) = command else {
            return Err("session renewal received another command".into());
        };
        let kuberic_runtime::protocol::command::PublicV1HistoryCommand::SessionRenewal {
            command: renewal_command,
        } = public_command.as_ref()
        else {
            return Err("session renewal received a history command".into());
        };
        let installs_configuration = matches!(
            renewal_command.as_ref(),
            kuberic_runtime::protocol::command::PublicV1SessionRenewalCommand::
                InstallFreshConfiguration { .. }
        );
        if installs_configuration {
            let authority = accepted_public_v1_status(observation.set.status.as_ref())?
                .ok_or_else(|| "session renewal lacks accepted public-v1 authority".to_string())?;
            loop {
                let replicas = renewal_topology_replicas(&observation, expected)?;
                // Reobserve retained completion on every invocation. A lost
                // Build response or controller recreation cannot skip revoke.
                if let Some(revoke) = renewal_completed_copy_revoke(expected, &replicas)? {
                    self.execute_command(&observation, &revoke)
                        .await
                        .map_err(|error| error.to_string())?;
                }
                let Some(preparation) = renewal_native_command(&authority, expected, &replicas)?
                else {
                    break;
                };
                if let ProtocolCommand::PublicV1Operation(operation) = &preparation
                    && let Some(PublicOperationProgram::Build(build)) =
                        operation.intent.program.as_ref()
                {
                    let source = replicas
                        .iter()
                        .find(|replica| replica.report.replica == operation.target)
                        .ok_or("native renewal copy lost its source")?;
                    let authority = PublicInboundBuildAuthority {
                        operation_id: operation.intent.operation_id.clone(),
                        operation_revision: operation.intent.revision,
                        attempt: PublicBuildAttemptReference {
                            resource_uid: scope.resource_uid.clone(),
                            epoch: build.epoch,
                            build_id: build.attempt.clone(),
                            source: operation.target.clone(),
                            source_storage: source.report.storage_incarnation.clone(),
                            source_session: operation.intent.process_session_id.clone(),
                            target: build.replica.clone(),
                            target_storage: build.target_storage.clone(),
                            target_session: build.process_session_id.clone(),
                        },
                        history: expected.required.history.clone(),
                        source_address: build.source_address.clone(),
                        current_configuration_id: build
                            .current_configuration_id
                            .clone()
                            .ok_or("native renewal copy lacks installed configuration")?,
                        target_address: build.replication_address.clone(),
                    };
                    self.execute_command(
                        &observation,
                        &ProtocolCommand::PublicV1History(Box::new(
                            kuberic_runtime::protocol::command::PublicV1HistoryCommand::InstallInboundBuildAuthority {
                                authority: Box::new(authority),
                            },
                        )),
                    ).await.map_err(|error| error.to_string())?;
                }
                self.execute_command(&observation, &preparation)
                    .await
                    .map_err(|error| error.to_string())?;
                observation = self.observe_public_v1_scope(scope).await?;
                if accepted_public_v1_renewal(observation.set.status.as_ref())?.as_ref()
                    != Some(expected)
                {
                    return Err("native renewal preparation lost its captured authority".into());
                }
            }
            let replicas = renewal_topology_replicas(&observation, expected)?;
            for configuration_command in
                renewal_configuration_commands(&authority, expected, &replicas)?
            {
                self.execute_command(&observation, &configuration_command)
                    .await
                    .map_err(|error| error.to_string())?;
            }
        }
        if !installs_configuration {
            self.execute_command(&observation, command)
                .await
                .map_err(|error| error.to_string())?;
        }
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
        let observation = self.observe_public_v1_status_scope(scope).await?;
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
        let sessions = topology_sessions(&observation, next)?;
        status.public_v1_topology = Some(topology_state_with_switchover_completion(
            &observation.set,
            &status,
            next,
            &sessions,
        )?);
        self.replace_public_v1_status(&observation, status).await
    }

    async fn persist_public_v1_primary_unavailable(
        &self,
        scope: &PublicV1Scope,
        expected: &kuberic_runtime::protocol::types::ConfigurationId,
        unavailable: Option<(ReplicaId, i64)>,
    ) -> std::result::Result<(), String> {
        let observation = self.observe_public_v1_status_scope(scope).await?;
        let mut status = observation.set.status.clone().unwrap_or_default();
        let topology = status
            .public_v1_topology
            .as_mut()
            .ok_or_else(|| "public-v1 topology is unavailable".to_string())?;
        if &topology.configuration_id != expected {
            return Err("stale public-v1 primary availability".into());
        }
        match unavailable {
            Some((primary, since)) => topology.record_primary_unavailable(primary, since),
            None => topology.clear_primary_unavailable(),
        }
        topology.configuration()?;
        self.replace_public_v1_status(&observation, status).await
    }

    async fn begin_public_v1_replacement(
        &self,
        scope: &PublicV1Scope,
        expected_topology: &kuberic_runtime::protocol::types::ConfigurationId,
        predecessor: &kuberic_runtime::protocol::types::ReplicaStorageIncarnationContext,
    ) -> std::result::Result<(), String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        let mut status = observation.set.status.clone().unwrap_or_default();
        let topology = status
            .public_v1_topology
            .as_ref()
            .ok_or_else(|| "replacement authority lacks public-v1 topology".to_string())?;
        if &topology.configuration_id != expected_topology
            || status.public_v1_replacement.is_some()
            || status.public_v1_transition.is_some()
            || status.public_v1_operation.is_some()
            || !topology
                .sessions
                .iter()
                .any(|session| session.storage_incarnation == *predecessor)
        {
            return Err("stale public-v1 replacement authority".into());
        }
        status.public_v1_replacement = Some(PublicV1ReplacementState::new(
            expected_topology,
            predecessor.clone(),
        )?);
        self.replace_public_v1_status(&observation, status).await
    }

    async fn bind_public_v1_replacement(
        &self,
        scope: &PublicV1Scope,
        expected_operation: &OperationId,
        replacement: &kuberic_runtime::protocol::types::ReplicaStorageIncarnationContext,
    ) -> std::result::Result<(), String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        let mut status = observation.set.status.clone().unwrap_or_default();
        let current = status
            .public_v1_replacement
            .as_ref()
            .ok_or_else(|| "public-v1 replacement authority disappeared".to_string())?;
        if &current.operation_id != expected_operation {
            return Err("stale public-v1 replacement binding".into());
        }
        status.public_v1_replacement = Some(current.bind(replacement.clone())?);
        self.replace_public_v1_status(&observation, status).await
    }

    async fn complete_public_v1_replacement(
        &self,
        scope: &PublicV1Scope,
        expected_operation: &OperationId,
    ) -> std::result::Result<(), String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        let mut status = observation.set.status.clone().unwrap_or_default();
        let current = status
            .public_v1_replacement
            .as_ref()
            .ok_or_else(|| "public-v1 replacement authority disappeared".to_string())?;
        if &current.operation_id != expected_operation
            || current.replacement.is_none()
            || status.public_v1_transition.is_some()
            || status.public_v1_operation.is_some()
        {
            return Err("stale public-v1 replacement completion".into());
        }
        status.public_v1_replacement = None;
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
        let observation = self.observe_public_v1_status_scope(scope).await?;
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

    async fn contain_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        command: &ProtocolCommand,
    ) -> std::result::Result<(), String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        if normalized_transition(&observation)?.as_ref() != Some(expected) {
            return Err("stale public-v1 containment authority".into());
        }
        revoke_inbound_build_authority(self, &observation, expected, command).await
    }

    async fn supersede_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        command: &ProtocolCommand,
    ) -> std::result::Result<(), String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        if !normalized_transition(&observation)?.is_some_and(|current| {
            current.evidence == expected.evidence && current.intent == expected.intent
        }) {
            return Err("stale public-v1 operation supersession".into());
        }
        revoke_inbound_build_authority(self, &observation, expected, command).await?;
        let mut status = observation.set.status.clone().unwrap_or_default();
        status.public_v1_transition = None;
        status.public_v1_operation = None;
        self.replace_public_v1_status(&observation, status).await
    }

    async fn public_v1_operation_source_present(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
    ) -> std::result::Result<bool, String> {
        let observation = self.observe_public_v1_scope(scope).await?;
        Ok(observation.pods.iter().any(|pod| {
            pod.uid().as_deref()
                == Some(
                    expected
                        .intent
                        .source
                        .storage_incarnation
                        .replica
                        .instance_id
                        .as_str(),
                )
                && pod.metadata.deletion_timestamp.is_none()
        }))
    }

    async fn observe_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
    ) -> std::result::Result<Option<PublicV1AcceptedTransitionStatus>, String> {
        let observation = self.observe_public_v1_status_scope(scope).await?;
        normalized_transition(&observation)
    }

    async fn dispatch_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        command: &ProtocolCommand,
    ) -> std::result::Result<(), String> {
        let operation = match command {
            ProtocolCommand::PublicV1Operation(operation) => Some(operation.as_ref()),
            _ => None,
        };
        let observation = if operation.is_some_and(|operation| {
            matches!(
                operation.intent.program,
                Some(PublicOperationProgram::Build(_))
            )
        }) {
            self.observe_public_v1_scope(scope).await?
        } else {
            let (target, _) = command_target(command);
            self.observe_public_v1_target_scope(scope, &target).await?
        };
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
        let identity = &expected.intent.source.storage_incarnation.replica;
        let observation = self.observe_public_v1_target_scope(scope, identity).await?;
        if normalized_transition(&observation)?.as_ref() != Some(expected) {
            return Err("stale public-v1 operation report authority".into());
        }
        let key = ReplicaObservationKey::new(identity.replica_id, identity.instance_id.clone());
        match observation.agents.get(&key) {
            Some(RawAgentObservation::PublicV1Report(report)) if report.replica == *identity => {
                Ok((**report).clone())
            }
            _ => Err("exact public-v1 operation report is unavailable".into()),
        }
    }

    async fn observe_public_v1_operation_source(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
    ) -> std::result::Result<RawAgentObservation, String> {
        let identity = &expected.intent.source.storage_incarnation.replica;
        let observation = self.observe_public_v1_target_scope(scope, identity).await?;
        if normalized_transition(&observation)?.as_ref() != Some(expected) {
            return Err("stale public-v1 operation source authority".into());
        }
        let key = ReplicaObservationKey::new(identity.replica_id, identity.instance_id.clone());
        let source = observation
            .agents
            .get(&key)
            .cloned()
            .ok_or_else(|| "exact public-v1 operation source was not observed".to_string())?;
        if let RawAgentObservation::PublicV1Report(report) = &source
            && (report.replica != *identity
                || report.resource_uid != scope.resource_uid
                || report.identity != expected.evidence.identity
                || report.history != expected.intent.history
                || report.storage_incarnation != expected.intent.source.storage_incarnation)
        {
            return Err("public-v1 operation source report changed exact authority".into());
        }
        Ok(source)
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
        let observation = self.observe_public_v1_status_scope(scope).await?;
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
        let observation = self.observe_public_v1_status_scope(scope).await?;
        if let Some(operation) = observation
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_operation.as_ref())
            .map(PublicV1OperationStatus::command)
            .transpose()?
            && let Some(PublicOperationProgram::Build(build)) = operation.intent.program.as_ref()
        {
            if expected.intent.consumer
                == crate::evaluator::public_v1_evidence::PublicV1EvidenceConsumer::RepairReplacement
            {
                let replacement = observation
                    .set
                    .status
                    .as_ref()
                    .and_then(|status| status.public_v1_replacement.as_ref())
                    .ok_or_else(|| "replacement build lacks durable authority".to_string())?;
                if replacement.replacement.as_ref() != Some(&build.target_storage) {
                    return Err("replacement build target differs from durable authority".into());
                }
            }
            let authority = inbound_build_authority(expected, &operation, build)?;
            let target_observation = self
                .observe_public_v1_target_scope(scope, &build.replica)
                .await?;
            self.execute_command(
                &target_observation,
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
        if let Some(command) = observation
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_operation.as_ref())
            .map(PublicV1OperationStatus::command)
            .transpose()?
        {
            if let Some(configuration) =
                crate::public_v1_reconciliation::ordinary_recovery_configuration(
                    &observation,
                    expected,
                    &command,
                )?
                && crate::public_v1_reconciliation::recovery_peer_changed(
                    &observation,
                    configuration,
                    expected,
                )?
            {
                return Err("ordinary recovery peer changed before source retirement".into());
            }
            revoke_inbound_build_authority(
                self,
                &observation,
                expected,
                &ProtocolCommand::PublicV1Operation(Box::new(command)),
            )
            .await?;
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
        let command = observation
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_operation.as_ref())
            .map(PublicV1OperationStatus::command)
            .transpose()?;
        let mut status = observation.set.status.clone().unwrap_or_default();
        let configuration_id = status
            .public_v1_topology
            .as_ref()
            .map(|topology| topology.configuration_id.clone())
            .ok_or_else(|| "corrective replacement lacks accepted topology".to_string())?;
        let action = plan_corrective_replacement(
            status.public_v1_replacement.as_ref(),
            &configuration_id,
            &target.storage_incarnation,
            replacement_target_is_admitted(&status, &target.storage_incarnation),
        )?;
        let target_pod_present =
            corrective_replacement_target_pod_present(&observation, &target.storage_incarnation);
        match action {
            CorrectiveReplacementAction::Persist {
                next,
                clear_transition,
            } => {
                if let Some(command) = command
                    && target_pod_present
                {
                    revoke_inbound_build_authority(
                        self,
                        &observation,
                        expected,
                        &ProtocolCommand::PublicV1Operation(Box::new(command)),
                    )
                    .await?;
                }
                status.public_v1_replacement = Some(next);
                if clear_transition {
                    status.public_v1_transition = None;
                    status.public_v1_operation = None;
                }
                self.replace_public_v1_status(&observation, status).await
            }
            CorrectiveReplacementAction::Cleanup { next } => {
                let storage = &target.storage_incarnation;
                if replica_storage_incarnation_remains(&observation, storage) {
                    let (pod_uid, pvc_uid) = match &storage.storage {
                        kuberic_runtime::protocol::types::ReplicaStorageIdentity::Persistent {
                            pod_uid,
                            pvc_uid,
                        } => (Some(pod_uid), Some(pvc_uid)),
                        kuberic_runtime::protocol::types::ReplicaStorageIdentity::Volatile {
                            ..
                        } => (None, None),
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
                    return Ok(());
                }
                status.public_v1_replacement = Some(next);
                status.public_v1_transition = None;
                status.public_v1_operation = None;
                self.replace_public_v1_status(&observation, status).await
            }
        }
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
        if let Some(primary) = primary {
            state
                .effects
                .push(EffectRecord::RoutePrimary(primary.clone()));
        }
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
        if let Some(uid) = pod_uid {
            state
                .observation
                .agents
                .retain(|key, _| key.instance_id.as_str() != uid.as_str());
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
            engine_idle: None,
            remote_builds: Vec::new(),
            retry_pending_operations: Vec::new(),
            active_operations: Vec::new(),
            configuration: None,
                            identity:
                                kuberic_runtime::protocol::public_operations::PublicV1Identity::new(1),
                            resource_uid: initialization.resource_uid.clone(),
                            replica: replica.clone(),
                            process_session_id: fresh.process_session_id,
                            replicator_creation_id:
                                kuberic_runtime::protocol::types::ReplicatorCreationId::new(
                                    format!("in-memory-creation-{}", replica.replica_id.value()),
                                ),
                            report_sequence: 1,
                            open_mode: kuberic_runtime::protocol::types::OpenMode::Existing,
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
                                current_progress: kuberic_runtime::protocol::public_operations::ProgressQueryResult::NotReady,
                                catch_up_capability: kuberic_runtime::protocol::public_operations::ProgressQueryResult::NotReady,
                            },
                            replication_address: None,
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
                    let (
                        call_kind,
                        previous_configuration_id,
                        current_configuration_id,
                        catch_up_mode,
                        catch_up_specific_quorum,
                    ) = in_memory_operation_metadata(&operation.intent)?;
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
                        catch_up_mode,
                        catch_up_specific_quorum,
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
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Open { .. } => {
                                report.open_mode = kuberic_runtime::protocol::types::OpenMode::Existing;
                                report.progress.current_progress =
                                    kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0);
                                report.progress.catch_up_capability =
                                    kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0);
                                report.extensions.catch_up_specific_quorum = Some(false);
                                report.replication_address = Some(format!(
                                    "http://10.0.0.{}:50052",
                                    report.replica.replica_id,
                                ));
                            }
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Role {
                                epoch,
                                role,
                            } => {
                                report.role = *role;
                                report.progress.role = match report.role {
                                    ReplicaRole::Primary => kuberic_runtime::protocol::public_operations::PublicProgressRole::Primary,
                                    ReplicaRole::ActiveSecondary | ReplicaRole::IdleSecondary => kuberic_runtime::protocol::public_operations::PublicProgressRole::Secondary,
                                    ReplicaRole::None => kuberic_runtime::protocol::public_operations::PublicProgressRole::Recovery,
                                };
                                if *role == ReplicaRole::IdleSecondary {
                                    report.progress.current_progress =
                                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::NotReady;
                                    report.progress.catch_up_capability =
                                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::NotReady;
                                }
                                report.read_status =
                                    kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
                                report.write_status =
                                    kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
                                report.service_location = None;
                                let _ = epoch;
                            }
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Configuration(configuration)
                            | kuberic_runtime::protocol::public_operations::PublicOperationProgram::CatchUp {
                                configuration,
                                ..
                            } => {
                                report.configuration = Some(kuberic_runtime::protocol::public_operations::PublicV1InstalledConfiguration { voting: configuration.clone() });
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
                                    report.read_status = kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
                                    report.write_status = kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
                                }
                            }
                            kuberic_runtime::protocol::public_operations::PublicOperationProgram::Swap {
                                handoff,
                                refreshed,
                                ..
                            } => {
                                report.configuration = Some(kuberic_runtime::protocol::public_operations::PublicV1InstalledConfiguration { voting: refreshed.clone() });
                                report.role = *handoff;
                                report.progress.role =
                                    kuberic_runtime::protocol::public_operations::PublicProgressRole::Secondary;
                                report.read_status =
                                    kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
                                report.write_status =
                                    kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
                                report.service_location = None;
                            }
                            _ => {}
                        }
                    }
                    report.report_sequence += 1;
                    report
                        .validate()
                        .map_err(|message| ControllerError::InvalidAgentEvidence(message.into()))?;
                    if let Some(
                        kuberic_runtime::protocol::public_operations::PublicOperationProgram::Build(
                            build,
                        ),
                    ) = operation.intent.program.as_ref()
                    {
                        let target = ReplicaObservationKey::new(
                            build.replica.replica_id,
                            build.replica.instance_id.clone(),
                        );
                        let Some(RawAgentObservation::PublicV1Report(report)) =
                            observation.agents.get_mut(&target)
                        else {
                            return Err(ControllerError::ObservationStale);
                        };
                        report.progress.current_progress =
                            kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0);
                        report.progress.catch_up_capability =
                            kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0);
                        report.report_sequence += 1;
                    }
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
            Option<kuberic_runtime::protocol::public_operations::PublicCatchUpMode>,
            Option<bool>,
        )> {
            use kuberic_runtime::protocol::public_operations::{
                PublicOperationProgram, PublicV1CallKind,
            };
            match intent.program.as_ref() {
                Some(PublicOperationProgram::Open { .. }) => {
                    Ok((PublicV1CallKind::Open, None, None, None, None))
                }
                Some(PublicOperationProgram::Role { .. }) => {
                    Ok((PublicV1CallKind::Role, None, None, None, None))
                }
                Some(PublicOperationProgram::Epoch { .. }) => {
                    Ok((PublicV1CallKind::Epoch, None, None, None, None))
                }
                Some(PublicOperationProgram::Configuration(configuration)) => Ok((
                    PublicV1CallKind::Configuration,
                    configuration
                        .previous
                        .as_ref()
                        .map(|previous| previous.configuration_id.clone()),
                    Some(configuration.current.configuration_id.clone()),
                    None,
                    None,
                )),
                Some(PublicOperationProgram::CatchUp {
                    configuration,
                    mode,
                }) => Ok((
                    PublicV1CallKind::CatchUp,
                    configuration
                        .previous
                        .as_ref()
                        .map(|previous| previous.configuration_id.clone()),
                    Some(configuration.current.configuration_id.clone()),
                    Some(*mode),
                    None,
                )),
                Some(PublicOperationProgram::Progress { .. }) => {
                    Ok((PublicV1CallKind::Progress, None, None, None, None))
                }
                Some(PublicOperationProgram::Swap {
                    refreshed,
                    mode,
                    catch_up_specific_quorum,
                    ..
                }) => Ok((
                    PublicV1CallKind::Swap,
                    refreshed
                        .previous
                        .as_ref()
                        .map(|previous| previous.configuration_id.clone()),
                    Some(refreshed.current.configuration_id.clone()),
                    Some(*mode),
                    Some(*catch_up_specific_quorum),
                )),
                Some(PublicOperationProgram::Build(build)) => Ok((
                    PublicV1CallKind::Build,
                    None,
                    build.current_configuration_id.clone(),
                    None,
                    None,
                )),
                Some(
                    PublicOperationProgram::Remove(_)
                    | PublicOperationProgram::RetireIdleReplica { .. },
                ) => Ok((PublicV1CallKind::Remove, None, None, None, None)),
                Some(PublicOperationProgram::Close) => {
                    Ok((PublicV1CallKind::Close, None, None, None, None))
                }
                Some(PublicOperationProgram::Abort) => {
                    Ok((PublicV1CallKind::Abort, None, None, None, None))
                }
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
    if !observation.failures.is_empty() {
        return Err(observation
            .failures
            .iter()
            .map(|failure| format!("{}: {}", failure.source, failure.message))
            .collect::<Vec<_>>()
            .join("; "));
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

    async fn complete_public_v1_session_renewal(
        &self,
        _scope: &PublicV1Scope,
        expected: &PublicV1SessionRenewalStatus,
    ) -> std::result::Result<(), String> {
        if expected.stage
            != kuberic_runtime::protocol::public_operations::PublicSessionRenewalStage::Authorized
        {
            return Err("public-v1 session renewal is not authorized".into());
        }
        let mut state = self.state.lock().await;
        let current_renewal = accepted_public_v1_renewal(state.observation.set.status.as_ref())?;
        if current_renewal.as_ref() != Some(expected) {
            return Err("stale in-memory public-v1 session-renewal completion".into());
        }
        let sessions = renewed_topology_sessions(&state.observation, expected)?;
        let observed_set = state.observation.set.clone();
        let status = state
            .observation
            .set
            .status
            .as_mut()
            .ok_or_else(|| "session renewal lacks in-memory status".to_string())?;
        let current = status
            .public_v1_topology
            .as_ref()
            .ok_or_else(|| "session renewal lacks in-memory topology".to_string())?
            .configuration()?;
        if current.epoch != expected.required.previous_epoch
            || status.public_v1_transition.is_some()
            || status.public_v1_operation.is_some()
        {
            return Err("session renewal changed its in-memory predecessor topology".into());
        }
        status.public_v1_topology = Some(topology_state_with_switchover_completion(
            &observed_set,
            status,
            &expected.required.fresh_configuration,
            &sessions,
        )?);
        status.public_v1_session_renewal = None;
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
        let ProtocolCommand::PublicV1History(public_command) = command else {
            return Err("session renewal received another command".into());
        };
        let kuberic_runtime::protocol::command::PublicV1HistoryCommand::SessionRenewal {
            command: renewal_command,
        } = public_command.as_ref()
        else {
            return Err("session renewal received a history command".into());
        };
        let expected_endpoint = renewal_command.expected_endpoint();
        let exact_report = observation.agents.values().find_map(|agent| match agent {
            RawAgentObservation::PublicV1Report(report)
                if report.replica == expected_endpoint.identity =>
            {
                Some(report.as_ref())
            }
            _ => None,
        });
        if exact_report.is_none_or(|report| {
            report.storage_incarnation != expected_endpoint.storage_incarnation
                || report.process_session_id != expected_endpoint.process_session_id
                || report.replicator_creation_id != expected_endpoint.replicator_creation_id
        }) {
            return Err("in-memory session-renewal endpoint differs from authority".into());
        }
        self.execute_command(&observation, command)
            .await
            .map_err(|error| error.to_string())?;
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
        if matches!(renewal_command.as_ref(),
            kuberic_runtime::protocol::command::PublicV1SessionRenewalCommand::ReplayReplacementRole { .. })
            && let Some(report) = exact_report.filter(|report| !report.has_current_open_endpoint())
        {
            let program = PublicOperationProgram::Open {
                replica: report.replica.clone(), existing: true,
            };
            let revision = accepted_public_v1_status(observation.set.status.as_ref())?
                .into_iter().flat_map(|authority| authority.completed_operations)
                .filter(|completion| completion.storage_incarnation.replica == report.replica)
                .map(|completion| completion.revision)
                .chain(report.completed_operations.iter().map(|completion| completion.revision))
                .max().unwrap_or(0).checked_add(1).ok_or("replacement Open revision exhausted")?;
            self.execute_command(&observation, &ProtocolCommand::PublicV1Operation(Box::new(
                PublicV1OperationCommand {
                    target: report.replica.clone(),
                    intent: PublicOperationIntent {
                        identity: report.identity.clone(),
                        operation_id: OperationId::new(format!(
                            "{}:replacement-open", expected.required.operation_id,
                        )),
                        revision,
                        process_session_id: report.process_session_id.clone(),
                        class: kuberic_runtime::protocol::public_operations::PublicOperationClass::Authority,
                        input_digest: program.digest(),
                        replay_disposition: program.replay_disposition(),
                        lifecycle: None,
                        program: Some(program),
                    },
                },
            ))).await.map_err(|error| error.to_string())?;
        }
        let mut state = self.state.lock().await;
        let target = renewal_command.target();
        let report = state
            .observation
            .agents
            .values_mut()
            .find_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report) if report.replica == *target => {
                    Some(report.as_mut())
                }
                _ => None,
            });
        match renewal_command.as_ref() {
            kuberic_runtime::protocol::command::PublicV1SessionRenewalCommand::CloseAccessAndIngress { .. } => {
                let report = report.ok_or_else(|| {
                    "session-renewal target report is unavailable".to_string()
                })?;
                report.read_status = kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
                report.write_status = kuberic_runtime::protocol::types::AccessStatus::NotPrimary;
                report.service_location = None;
            }
            kuberic_runtime::protocol::command::PublicV1SessionRenewalCommand::ReplayReplacementRole {
                role,
                ..
            } => {
                let report = report.ok_or_else(|| {
                    "session-renewal replacement report is unavailable".to_string()
                })?;
                report.role = *role;
                report.progress.role = match role {
                    ReplicaRole::Primary => {
                        kuberic_runtime::protocol::public_operations::PublicProgressRole::Primary
                    }
                    ReplicaRole::ActiveSecondary => {
                        kuberic_runtime::protocol::public_operations::PublicProgressRole::Secondary
                    }
                    _ => {
                        kuberic_runtime::protocol::public_operations::PublicProgressRole::Recovery
                    }
                };
                report.read_status = kuberic_runtime::protocol::types::AccessStatus::Granted;
                report.write_status = if *role == ReplicaRole::Primary {
                    kuberic_runtime::protocol::types::AccessStatus::Granted
                } else {
                    kuberic_runtime::protocol::types::AccessStatus::NotPrimary
                };
            }
            _ => {}
        }
        let status = state
            .observation
            .set
            .status
            .as_mut()
            .ok_or_else(|| "session renewal lacks in-memory status".to_string())?;
        status.public_v1_session_renewal = Some(PublicV1SessionRenewalState::from_renewal(&next)?);
        advance_in_memory_resource_version(&mut state);
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
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
        let sessions = topology_sessions(&state.observation, next)?;
        let observed_set = state.observation.set.clone();
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
        status.public_v1_topology = Some(topology_state_with_switchover_completion(
            &observed_set,
            status,
            next,
            &sessions,
        )?);
        advance_in_memory_resource_version(&mut state);
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
    }

    async fn persist_public_v1_primary_unavailable(
        &self,
        _scope: &PublicV1Scope,
        expected: &kuberic_runtime::protocol::types::ConfigurationId,
        unavailable: Option<(ReplicaId, i64)>,
    ) -> std::result::Result<(), String> {
        let mut state = self.state.lock().await;
        let topology = state
            .observation
            .set
            .status
            .as_mut()
            .and_then(|status| status.public_v1_topology.as_mut())
            .ok_or_else(|| "public-v1 topology is unavailable".to_string())?;
        if &topology.configuration_id != expected {
            return Err("stale public-v1 primary availability".into());
        }
        match unavailable {
            Some((primary, since)) => topology.record_primary_unavailable(primary, since),
            None => topology.clear_primary_unavailable(),
        }
        topology.configuration()?;
        advance_in_memory_resource_version(&mut state);
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
    }

    async fn begin_public_v1_replacement(
        &self,
        _scope: &PublicV1Scope,
        expected_topology: &kuberic_runtime::protocol::types::ConfigurationId,
        predecessor: &kuberic_runtime::protocol::types::ReplicaStorageIncarnationContext,
    ) -> std::result::Result<(), String> {
        let mut state = self.state.lock().await;
        let status = state
            .observation
            .set
            .status
            .as_mut()
            .ok_or_else(|| "replacement authority lacks in-memory status".to_string())?;
        let topology = status
            .public_v1_topology
            .as_ref()
            .ok_or_else(|| "replacement authority lacks in-memory topology".to_string())?;
        if &topology.configuration_id != expected_topology
            || status.public_v1_replacement.is_some()
            || status.public_v1_transition.is_some()
            || status.public_v1_operation.is_some()
            || !topology
                .sessions
                .iter()
                .any(|session| session.storage_incarnation == *predecessor)
        {
            return Err("stale in-memory public-v1 replacement authority".into());
        }
        status.public_v1_replacement = Some(PublicV1ReplacementState::new(
            expected_topology,
            predecessor.clone(),
        )?);
        advance_in_memory_resource_version(&mut state);
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
    }

    async fn bind_public_v1_replacement(
        &self,
        _scope: &PublicV1Scope,
        expected_operation: &OperationId,
        replacement: &kuberic_runtime::protocol::types::ReplicaStorageIncarnationContext,
    ) -> std::result::Result<(), String> {
        let mut state = self.state.lock().await;
        let status = state
            .observation
            .set
            .status
            .as_mut()
            .ok_or_else(|| "replacement binding lacks in-memory status".to_string())?;
        let current = status
            .public_v1_replacement
            .as_ref()
            .ok_or_else(|| "in-memory public-v1 replacement authority disappeared".to_string())?;
        if &current.operation_id != expected_operation {
            return Err("stale in-memory public-v1 replacement binding".into());
        }
        status.public_v1_replacement = Some(current.bind(replacement.clone())?);
        advance_in_memory_resource_version(&mut state);
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
    }

    async fn complete_public_v1_replacement(
        &self,
        _scope: &PublicV1Scope,
        expected_operation: &OperationId,
    ) -> std::result::Result<(), String> {
        let mut state = self.state.lock().await;
        let status = state
            .observation
            .set
            .status
            .as_mut()
            .ok_or_else(|| "replacement completion lacks in-memory status".to_string())?;
        let current = status
            .public_v1_replacement
            .as_ref()
            .ok_or_else(|| "in-memory public-v1 replacement authority disappeared".to_string())?;
        if &current.operation_id != expected_operation
            || current.replacement.is_none()
            || status.public_v1_transition.is_some()
            || status.public_v1_operation.is_some()
        {
            return Err("stale in-memory public-v1 replacement completion".into());
        }
        status.public_v1_replacement = None;
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

    async fn supersede_public_v1_operation(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
        _command: &ProtocolCommand,
    ) -> std::result::Result<(), String> {
        let mut state = self.state.lock().await;
        validate_in_memory_public_v1_scope(&state.observation, scope)?;
        if !normalized_transition(&state.observation)?.is_some_and(|current| {
            current.evidence == expected.evidence && current.intent == expected.intent
        }) {
            return Err("stale in-memory public-v1 operation supersession".into());
        }
        let status = state.observation.set.status.get_or_insert_default();
        status.public_v1_transition = None;
        status.public_v1_operation = None;
        advance_in_memory_resource_version(&mut state);
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
    }

    async fn public_v1_operation_source_present(
        &self,
        scope: &PublicV1Scope,
        expected: &PublicV1AcceptedTransitionStatus,
    ) -> std::result::Result<bool, String> {
        let state = self.state.lock().await;
        validate_in_memory_public_v1_scope(&state.observation, scope)?;
        Ok(state.observation.pods.iter().any(|pod| {
            pod.uid().as_deref()
                == Some(
                    expected
                        .intent
                        .source
                        .storage_incarnation
                        .replica
                        .instance_id
                        .as_str(),
                )
                && pod.metadata.deletion_timestamp.is_none()
        }))
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
        if expected.intent.consumer
            == crate::evaluator::public_v1_evidence::PublicV1EvidenceConsumer::RepairReplacement
        {
            let state = self.state.lock().await;
            let status = state
                .observation
                .set
                .status
                .as_ref()
                .ok_or_else(|| "replacement build lacks in-memory status".to_string())?;
            let command = status
                .public_v1_operation
                .as_ref()
                .ok_or_else(|| "replacement build lacks in-memory operation".to_string())?
                .command()?;
            let Some(PublicOperationProgram::Build(build)) = command.intent.program.as_ref() else {
                return Err("replacement completion is not a build".into());
            };
            if status
                .public_v1_replacement
                .as_ref()
                .and_then(|replacement| replacement.replacement.as_ref())
                != Some(&build.target_storage)
            {
                return Err("replacement build target differs from in-memory authority".into());
            }
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
        target: &PublicV1ResourceAuthority,
        rebuild: &CanonicalReplicaRebuildRequired,
    ) -> std::result::Result<(), String> {
        if !expected.intent.matches(completion)
            || !matches!(
                &completion.result,
                PublicV1OperationResult::RebuildRequired(observed)
                    if observed.as_ref() == rebuild
            )
        {
            return Err("in-memory public-v1 replacement differs from intent".into());
        }
        let mut state = self.state.lock().await;
        validate_in_memory_public_v1_scope(&state.observation, scope)?;
        if normalized_transition(&state.observation)?.as_ref() != Some(expected) {
            return Err("stale in-memory corrective replacement".into());
        }
        let current_replacement = state
            .observation
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_replacement.clone());
        let configuration_id = state
            .observation
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .map(|topology| topology.configuration_id.clone())
            .ok_or_else(|| "corrective replacement lacks accepted topology".to_string())?;
        let action = plan_corrective_replacement(
            current_replacement.as_ref(),
            &configuration_id,
            &target.storage_incarnation,
            state.observation.set.status.as_ref().is_some_and(|status| {
                replacement_target_is_admitted(status, &target.storage_incarnation)
            }),
        )?;
        match action {
            CorrectiveReplacementAction::Persist {
                next,
                clear_transition,
            } => {
                if state.conflict_next_status {
                    state.conflict_next_status = false;
                    return Err("injected in-memory public-v1 status conflict".into());
                }
                let status = state
                    .observation
                    .set
                    .status
                    .as_mut()
                    .expect("status checked above");
                status.public_v1_replacement = Some(next);
                if clear_transition {
                    status.public_v1_transition = None;
                    status.public_v1_operation = None;
                }
            }
            CorrectiveReplacementAction::Cleanup { next } => {
                if replica_storage_incarnation_remains(
                    &state.observation,
                    &target.storage_incarnation,
                ) {
                    if let kuberic_runtime::protocol::types::ReplicaStorageIdentity::Persistent {
                        pod_uid,
                        pvc_uid,
                    } = &target.storage_incarnation.storage
                    {
                        state.observation.services.retain(|service| {
                            !service.spec.as_ref().is_some_and(|spec| {
                                spec.selector.as_ref().is_some_and(|selector| {
                                    selector
                                        .get(INSTANCE_LABEL)
                                        .is_some_and(|instance| instance == pod_uid.as_str())
                                })
                            })
                        });
                        state
                            .observation
                            .pods
                            .retain(|pod| pod.uid().as_deref() != Some(pod_uid.as_str()));
                        state
                            .observation
                            .pvcs
                            .retain(|pvc| pvc.uid().as_deref() != Some(pvc_uid.as_str()));
                    }
                    state.observation.agents.retain(|key, _| {
                        key.replica_id != target.storage_incarnation.replica.replica_id
                            || key.instance_id != target.storage_incarnation.replica.instance_id
                    });
                    state.effects.push(EffectRecord::DeleteScaffolding {
                        pod_name: None,
                        pvc_name: None,
                    });
                    return Ok(());
                }
                if state.conflict_next_status {
                    state.conflict_next_status = false;
                    return Err("injected in-memory public-v1 status conflict".into());
                }
                let status = state
                    .observation
                    .set
                    .status
                    .as_mut()
                    .expect("status checked above");
                status.public_v1_replacement = Some(next);
                status.public_v1_transition = None;
                status.public_v1_operation = None;
            }
        }
        advance_in_memory_resource_version(&mut state);
        state.effects.push(EffectRecord::ReplaceStatus);
        Ok(())
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

#[cfg(all(test, feature = "runtime-test-bridge"))]
mod corrective_replacement_tests {
    use super::*;
    use crate::crd::{KubericSetSpec, PublicV1TopologyState};
    use crate::evaluator::public_v1_evidence::{
        PublicV1AcceptedTransitionIntent, PublicV1EvidenceConsumer,
    };
    use kuberic_runtime::protocol::public_operations::{
        CapturedPublicOperationIdentity, ProviderReplicaIdentity, PublicBuildInput,
        PublicOperationClass, PublicOperationIntent, PublicOperationProgram, PublicV1CallKind,
        PublicV1Identity, ReplicaRebuildReason,
    };
    use kuberic_runtime::protocol::types::{
        AgentGeneration, ConfigurationMember, Epoch, ProcessSessionId, PublicProtocolGeneration,
        ReplicaStorageIdentity, ReplicatorCreationId, StorageGeneration, derive_bootstrap_history,
        derive_history_initialization_id,
    };

    fn storage(instance: &str) -> ReplicaStorageIncarnationContext {
        ReplicaStorageIncarnationContext {
            replica: ReplicaIdentity {
                replica_id: ReplicaId::new(1),
                instance_id: ReplicaInstanceId::new(instance),
                agent_generation: AgentGeneration::new(format!("generation-{instance}")),
            },
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new(instance),
                pvc_uid: PvcUid::new(format!("pvc-{instance}")),
            },
            storage_generation: StorageGeneration::new(format!("storage-{instance}")),
        }
    }

    fn accepted_status() -> PublicV1AcceptedStatus {
        let resource_uid = ResourceUid::new("resource-1");
        let generation = PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        PublicV1AcceptedStatus {
            identity: PublicV1Identity::new(1),
            history_initialization_id: initialization.clone(),
            history: derive_bootstrap_history(&resource_uid, generation, &initialization),
            data_loss_recovery: None,
            completed_operations: Vec::new(),
            extensions: Default::default(),
        }
    }

    fn configuration(target: &ReplicaStorageIncarnationContext) -> ConfigurationDescriptor {
        ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            ReplicaId::new(1),
            vec![ConfigurationMember {
                identity: target.replica.clone(),
                role: ReplicaRole::Primary,
            }],
            1,
        )
    }

    fn build_program(
        accepted: &PublicV1AcceptedStatus,
        target: &ReplicaStorageIncarnationContext,
        operation_id: &OperationId,
        process_session_id: &ProcessSessionId,
        configuration_id: &ConfigurationId,
    ) -> PublicOperationProgram {
        PublicOperationProgram::Build(PublicBuildInput {
            attempt: operation_id.clone(),
            source_address: "http://127.0.0.1:50052".into(),
            epoch: Epoch::new(accepted.history.data_loss_number, 1),
            current_configuration_id: Some(configuration_id.clone()),
            replica: target.replica.clone(),
            target_storage: target.clone(),
            process_session_id: process_session_id.clone(),
            replication_address: "http://127.0.0.1:50052".into(),
            history: Some(accepted.history.clone()),
        })
    }

    fn corrective_inputs(
        target: &ReplicaStorageIncarnationContext,
    ) -> (
        PublicV1AcceptedTransitionStatus,
        PublicV1OperationCompletion,
        PublicV1ResourceAuthority,
        CanonicalReplicaRebuildRequired,
    ) {
        let accepted = accepted_status();
        let configuration_id = configuration(target).configuration_id;
        let operation_id = OperationId::new("corrective-rebuild");
        let process_session_id =
            kuberic_runtime::protocol::types::ProcessSessionId::new("corrective-session");
        let program = build_program(
            &accepted,
            target,
            &operation_id,
            &process_session_id,
            &configuration_id,
        );
        let input_digest = program.digest();
        let replay_disposition = program.replay_disposition();
        let rebuild = CanonicalReplicaRebuildRequired::LocalProvider {
            reason: ReplicaRebuildReason::ProviderMismatch,
            provider: ProviderReplicaIdentity {
                replica: target.replica.clone(),
            },
            operation: CapturedPublicOperationIdentity {
                resource_uid: accepted.history.resource_uid.clone(),
                identity: accepted.identity.clone(),
                epoch: Epoch::new(0, 1),
                operation_id: operation_id.clone(),
                revision: 1,
                detecting_session: process_session_id.clone(),
            },
            local_replica: target.replica.clone(),
            local_storage: match &target.storage {
                ReplicaStorageIdentity::Persistent { pvc_uid, .. } => pvc_uid.clone(),
                ReplicaStorageIdentity::Volatile { .. } => unreachable!(),
            },
        };
        let authority = PublicV1ResourceAuthority {
            storage_incarnation: target.clone(),
            resource_version: "target-rv".into(),
        };
        let intent = PublicV1AcceptedTransitionIntent {
            consumer: PublicV1EvidenceConsumer::RepairReplacement,
            operation_id: operation_id.clone(),
            revision: 1,
            call_kind: PublicV1CallKind::Build,
            history: accepted.history.clone(),
            source: authority.clone(),
            target: None,
            process_session_id: process_session_id.clone(),
            input_digest: input_digest.clone(),
            replay_disposition,
            previous_configuration_id: None,
            current_configuration_id: Some(configuration_id.clone()),
            catch_up_mode: None,
            catch_up_specific_quorum: None,
        };
        let expected = PublicV1AcceptedTransitionStatus {
            resource_version: "1".into(),
            evidence: accepted.clone(),
            intent,
        };
        let completion = PublicV1OperationCompletion {
            operation_id,
            revision: 1,
            call_kind: PublicV1CallKind::Build,
            process_session_id,
            input_digest,
            replay_disposition,
            history: accepted.history,
            storage_incarnation: target.clone(),
            previous_configuration_id: None,
            current_configuration_id: Some(configuration_id),
            catch_up_mode: None,
            catch_up_specific_quorum: None,
            result: PublicV1OperationResult::RebuildRequired(Box::new(rebuild.clone())),
        };
        (expected, completion, authority, rebuild)
    }

    fn corrective_observation(
        authority: PublicV1ReplacementState,
        expected: &PublicV1AcceptedTransitionStatus,
        target: &ReplicaStorageIncarnationContext,
    ) -> RawObservation {
        let mut set = KubericSet::new(
            "sample",
            KubericSetSpec {
                replicas: 1,
                image: "example".into(),
                failover_delay_seconds: 30,
                switchover: None,
            },
        );
        set.metadata.namespace = Some("default".into());
        set.metadata.uid = Some("resource-1".into());
        set.metadata.resource_version = Some("1".into());
        let endpoint = ProcessSessionEndpoint {
            storage_incarnation: target.clone(),
            identity: target.replica.clone(),
            process_session_id: ProcessSessionId::new("corrective-session"),
            replicator_creation_id: ReplicatorCreationId::new("corrective-creation"),
            role: ReplicaRole::Primary,
        };
        let configuration = configuration(target);
        let program = build_program(
            &expected.evidence,
            target,
            &expected.intent.operation_id,
            &expected.intent.process_session_id,
            expected
                .intent
                .current_configuration_id
                .as_ref()
                .expect("build configuration"),
        );
        let command = PublicV1OperationCommand {
            target: target.replica.clone(),
            intent: PublicOperationIntent {
                identity: expected.evidence.identity.clone(),
                operation_id: expected.intent.operation_id.clone(),
                revision: expected.intent.revision,
                process_session_id: expected.intent.process_session_id.clone(),
                class: PublicOperationClass::Build {
                    target: target.replica.replica_id,
                },
                input_digest: expected.intent.input_digest.clone(),
                replay_disposition: expected.intent.replay_disposition,
                lifecycle: None,
                program: Some(program),
            },
        };
        set.status = Some(KubericSetStatus {
            public_v1_authority: Some(
                PublicV1AuthorityStatus::from_authority(&expected.evidence).unwrap(),
            ),
            public_v1_topology: Some(
                PublicV1TopologyState::from_configuration(&configuration, &[endpoint]).unwrap(),
            ),
            public_v1_replacement: Some(authority),
            public_v1_transition: Some(PublicV1TransitionState::from_transition(expected).unwrap()),
            public_v1_operation: Some(PublicV1OperationStatus::from_command(&command).unwrap()),
            ..Default::default()
        });
        let (pod_uid, pvc_uid) = match &target.storage {
            ReplicaStorageIdentity::Persistent { pod_uid, pvc_uid } => (pod_uid, pvc_uid),
            ReplicaStorageIdentity::Volatile { .. } => unreachable!(),
        };
        let pod = Pod {
            metadata: kube::core::ObjectMeta {
                name: Some("sample-1".into()),
                namespace: Some("default".into()),
                uid: Some(pod_uid.to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let pvc = PersistentVolumeClaim {
            metadata: kube::core::ObjectMeta {
                name: Some("sample-1-data".into()),
                namespace: Some("default".into()),
                uid: Some(pvc_uid.to_string()),
                ..Default::default()
            },
            ..Default::default()
        };
        let service = Service {
            metadata: kube::core::ObjectMeta {
                name: Some("replacement-service".into()),
                namespace: Some("default".into()),
                uid: Some("replacement-service-uid".into()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(BTreeMap::from([(
                    INSTANCE_LABEL.into(),
                    pod_uid.to_string(),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        };
        let peer_service = Service {
            metadata: kube::core::ObjectMeta {
                name: Some("sample-peer".into()),
                namespace: Some("default".into()),
                uid: Some("peer-service-uid".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let write_service = Service {
            metadata: kube::core::ObjectMeta {
                name: Some("sample-write".into()),
                namespace: Some("default".into()),
                uid: Some("write-service-uid".into()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(BTreeMap::from([(INSTANCE_LABEL.into(), "disabled".into())])),
                ..Default::default()
            }),
            ..Default::default()
        };
        let credentials = Secret {
            metadata: kube::core::ObjectMeta {
                name: Some("sample-agent-credentials".into()),
                namespace: Some("default".into()),
                uid: Some("credentials-uid".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        RawObservation {
            set,
            pods: vec![pod],
            pvcs: vec![pvc],
            services: vec![service, peer_service, write_service],
            secrets: vec![credentials],
            agents: BTreeMap::new(),
            failures: Vec::new(),
            now_unix_seconds: 0,
        }
    }

    fn corrective_report(
        expected: &PublicV1AcceptedTransitionStatus,
        completion: &PublicV1OperationCompletion,
        target: &ReplicaStorageIncarnationContext,
    ) -> PublicV1AgentReport {
        PublicV1AgentReport {
            engine_idle: None,
            remote_builds: Vec::new(),
            retry_pending_operations: Vec::new(),
            active_operations: Vec::new(),
            configuration: None,
            identity: expected.evidence.identity.clone(),
            resource_uid: expected.evidence.history.resource_uid.clone(),
            replica: target.replica.clone(),
            process_session_id: expected.intent.process_session_id.clone(),
            replicator_creation_id: ReplicatorCreationId::new("corrective-creation"),
            report_sequence: 1,
            open_mode: kuberic_runtime::protocol::types::OpenMode::Existing,
            role: ReplicaRole::Primary,
            read_status: kuberic_runtime::protocol::types::AccessStatus::Granted,
            write_status: kuberic_runtime::protocol::types::AccessStatus::Granted,
            history: expected.evidence.history.clone(),
            storage_incarnation: target.clone(),
            progress: kuberic_runtime::protocol::public_operations::PublicProgress {
                role: kuberic_runtime::protocol::public_operations::PublicProgressRole::Primary,
                current_progress:
                    kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0),
                catch_up_capability:
                    kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0),
            },
            replication_address: Some("http://10.0.0.1:50052".into()),
            service_location: None,
            data_loss_recovery: None,
            completed_operations: vec![completion.clone()],
            extensions: Default::default(),
        }
    }

    #[test]
    fn corrective_replacement_planning_rejects_crossed_authority_before_cleanup() {
        let predecessor = storage("predecessor");
        let target = storage("target");
        let crossed = storage("crossed");
        let configuration_id = ConfigurationId::new("configuration");
        let authority = PublicV1ReplacementState::new(&configuration_id, predecessor)
            .unwrap()
            .bind(target.clone())
            .unwrap();

        assert!(
            plan_corrective_replacement(Some(&authority), &configuration_id, &crossed, false,)
                .is_err()
        );
        let CorrectiveReplacementAction::Persist {
            next,
            clear_transition,
        } = plan_corrective_replacement(Some(&authority), &configuration_id, &target, true)
            .unwrap()
        else {
            panic!("bound target must first persist retirement")
        };
        assert!(!clear_transition);
        assert!(next.replacement.is_none());
        assert_eq!(next.retiring_replacement.as_ref(), Some(&target));
        let CorrectiveReplacementAction::Cleanup { next: unbound } =
            plan_corrective_replacement(Some(&next), &configuration_id, &target, true).unwrap()
        else {
            panic!("retiring target must proceed to exact cleanup")
        };
        assert_eq!(unbound.predecessor, target);
        assert_ne!(unbound.operation_id, authority.operation_id);
        assert!(unbound.replacement.is_none());
        assert!(unbound.retiring_replacement.is_none());
        assert!(unbound.bind(storage("fresh")).is_ok());
        let CorrectiveReplacementAction::Cleanup {
            next: preadmission_unbound,
        } = plan_corrective_replacement(Some(&next), &configuration_id, &target, false).unwrap()
        else {
            panic!("pre-admission cleanup must preserve predecessor authority")
        };
        assert_eq!(preadmission_unbound.predecessor, authority.predecessor);
        assert_eq!(preadmission_unbound.operation_id, authority.operation_id);
    }

    #[tokio::test]
    async fn corrective_replacement_status_conflict_precedes_delete_and_cleanup_retries() {
        let predecessor = storage("predecessor");
        let target = storage("target");
        let configuration_id = ConfigurationId::new("configuration");
        let authority = PublicV1ReplacementState::new(&configuration_id, predecessor)
            .unwrap()
            .bind(target.clone())
            .unwrap();
        let (expected, completion, target_authority, rebuild) = corrective_inputs(&target);
        let observation = corrective_observation(authority.clone(), &expected, &target);
        assert!(corrective_replacement_target_pod_present(
            &observation,
            &target
        ));
        assert!(
            observation.pods[0]
                .metadata
                .annotations
                .as_ref()
                .is_none_or(|annotations| !annotations.contains_key(CONTROL_ADDRESS_ANNOTATION)),
            "fixture must prove an addressless present Pod still requires revocation"
        );
        let mut absent_pod = observation.clone();
        absent_pod.pods.clear();
        assert!(!corrective_replacement_target_pod_present(
            &absent_pod,
            &target
        ));
        assert!(!absent_pod.pvcs.is_empty() && !absent_pod.services.is_empty());
        let api = InMemoryClusterApi::new(observation);
        let scope = PublicV1Scope {
            namespace: "default".into(),
            name: "sample".into(),
            resource_uid: ResourceUid::new("resource-1"),
        };

        api.conflict_next_status().await;
        assert!(
            api.provision_public_v1_replacement(
                &scope,
                &expected,
                &completion,
                &target_authority,
                &rebuild,
            )
            .await
            .is_err()
        );
        let observed = api.observation().await;
        assert_eq!(observed.pods.len(), 1);
        assert_eq!(observed.pvcs.len(), 1);
        assert_eq!(observed.services.len(), 3);
        assert_eq!(
            observed
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_replacement.as_ref()),
            Some(&authority)
        );
        assert!(api.effects().await.is_empty());

        api.provision_public_v1_replacement(
            &scope,
            &expected,
            &completion,
            &target_authority,
            &rebuild,
        )
        .await
        .unwrap();
        let retiring = api.observation().await;
        assert_eq!(retiring.pods.len(), 1);
        assert_eq!(retiring.pvcs.len(), 1);
        assert_eq!(retiring.services.len(), 3);
        let retiring_authority = retiring
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_replacement.as_ref())
            .unwrap();
        assert!(retiring_authority.replacement.is_none());
        assert_eq!(
            retiring_authority.retiring_replacement.as_ref(),
            Some(&target)
        );
        assert!(
            retiring
                .set
                .status
                .as_ref()
                .unwrap()
                .public_v1_transition
                .is_some()
        );
        let cleanup_expected = normalized_transition(&retiring).unwrap().unwrap();

        api.provision_public_v1_replacement(
            &scope,
            &cleanup_expected,
            &completion,
            &target_authority,
            &rebuild,
        )
        .await
        .unwrap();
        let cleanup_pending = api.observation().await;
        assert!(cleanup_pending.pods.is_empty());
        assert!(cleanup_pending.pvcs.is_empty());
        assert_eq!(cleanup_pending.services.len(), 2);
        assert!(
            cleanup_pending
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_replacement.as_ref())
                .and_then(|replacement| replacement.retiring_replacement.as_ref())
                .is_some()
        );
        assert!(
            cleanup_pending
                .set
                .status
                .as_ref()
                .unwrap()
                .public_v1_transition
                .is_some()
        );

        let mut delayed_resources = cleanup_pending.clone();
        delayed_resources.pods = retiring.pods.clone();
        delayed_resources.pvcs = retiring.pvcs.clone();
        delayed_resources.services = retiring.services.clone();
        api.set_observation(delayed_resources).await;
        api.provision_public_v1_replacement(
            &scope,
            &cleanup_expected,
            &completion,
            &target_authority,
            &rebuild,
        )
        .await
        .unwrap();
        let delayed_cleanup = api.observation().await;
        assert!(delayed_cleanup.pods.is_empty());
        assert!(delayed_cleanup.pvcs.is_empty());
        assert_eq!(delayed_cleanup.services.len(), 2);
        assert!(
            delayed_cleanup
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_replacement.as_ref())
                .and_then(|replacement| replacement.retiring_replacement.as_ref())
                .is_some()
        );

        let mut lingering_endpoint = delayed_cleanup;
        lingering_endpoint.agents.insert(
            ReplicaObservationKey::new(
                target.replica.replica_id,
                target.replica.instance_id.clone(),
            ),
            RawAgentObservation::Unavailable {
                message: "terminating endpoint".into(),
            },
        );
        api.set_observation(lingering_endpoint).await;
        api.provision_public_v1_replacement(
            &scope,
            &cleanup_expected,
            &completion,
            &target_authority,
            &rebuild,
        )
        .await
        .unwrap();
        let endpoint_cleanup = api.observation().await;
        assert!(endpoint_cleanup.agents.is_empty());
        assert!(
            endpoint_cleanup
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_replacement.as_ref())
                .and_then(|replacement| replacement.retiring_replacement.as_ref())
                .is_some()
        );

        api.conflict_next_status().await;
        assert!(
            api.provision_public_v1_replacement(
                &scope,
                &cleanup_expected,
                &completion,
                &target_authority,
                &rebuild,
            )
            .await
            .is_err()
        );
        assert!(
            api.observation()
                .await
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_replacement.as_ref())
                .and_then(|replacement| replacement.retiring_replacement.as_ref())
                .is_some()
        );

        api.provision_public_v1_replacement(
            &scope,
            &cleanup_expected,
            &completion,
            &target_authority,
            &rebuild,
        )
        .await
        .unwrap();
        let complete = api.observation().await;
        let status = complete.set.status.as_ref().unwrap();
        let replacement = status.public_v1_replacement.as_ref().unwrap();
        assert_eq!(replacement.predecessor, target);
        assert_ne!(replacement.operation_id, authority.operation_id);
        assert!(replacement.replacement.is_none());
        assert!(replacement.retiring_replacement.is_none());
        assert!(status.public_v1_transition.is_none());
        assert!(status.public_v1_operation.is_none());
        status.validate_public_v1().unwrap();

        let fresh = storage("fresh");
        assert!(
            api.bind_public_v1_replacement(&scope, &authority.operation_id, &fresh)
                .await
                .is_err()
        );
        api.bind_public_v1_replacement(&scope, &replacement.operation_id, &fresh)
            .await
            .unwrap();
        let rebound = api.observation().await;
        let rebound_status = rebound.set.status.as_ref().unwrap();
        assert_eq!(
            rebound_status
                .public_v1_replacement
                .as_ref()
                .and_then(|replacement| replacement.replacement.as_ref()),
            Some(&fresh)
        );
        rebound_status.validate_public_v1().unwrap();
    }

    #[tokio::test]
    async fn reconciler_prioritizes_corrective_reentry_and_allocates_with_live_predecessor() {
        let predecessor = storage("predecessor");
        let target = storage("target");
        let configuration_id = ConfigurationId::new("configuration");
        let bound = PublicV1ReplacementState::new(&configuration_id, predecessor)
            .unwrap()
            .bind(target.clone())
            .unwrap();
        let (expected, completion, target_authority, rebuild) = corrective_inputs(&target);
        let mut observation = corrective_observation(bound, &expected, &target);
        observation.agents.insert(
            ReplicaObservationKey::new(
                target.replica.replica_id,
                target.replica.instance_id.clone(),
            ),
            RawAgentObservation::PublicV1Report(Box::new(corrective_report(
                &expected,
                &completion,
                &target,
            ))),
        );
        let api = Arc::new(InMemoryClusterApi::new(observation));
        let reconciler = crate::reconciler::Reconciler::new(
            api.clone(),
            crate::production_evaluation_config(0, 1, 1),
        );

        for _ in 0..4 {
            reconciler.reconcile("default", "sample").await.unwrap();
            if api
                .observation()
                .await
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_replacement.as_ref())
                .and_then(|replacement| replacement.retiring_replacement.as_ref())
                == Some(&target)
            {
                break;
            }
        }
        let reentry = api.observation().await;
        assert_eq!(
            reentry
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_replacement.as_ref())
                .and_then(|replacement| replacement.retiring_replacement.as_ref()),
            Some(&target)
        );

        let mut initial = corrective_observation(
            PublicV1ReplacementState::new(&configuration_id, target.clone()).unwrap(),
            &expected,
            &target,
        );
        initial.set.status.as_mut().unwrap().public_v1_replacement = None;
        initial.agents.insert(
            ReplicaObservationKey::new(
                target.replica.replica_id,
                target.replica.instance_id.clone(),
            ),
            RawAgentObservation::PublicV1Report(Box::new(corrective_report(
                &expected,
                &completion,
                &target,
            ))),
        );
        let initial_topology = initial
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .unwrap()
            .configuration()
            .unwrap();
        assert!(!crate::reconciler::bootstrap_resources_missing(
            &initial,
            &[ReplicaId::new(1)],
            Some(&initial_topology),
        ));
        let api = Arc::new(InMemoryClusterApi::new(initial));
        api.provision_public_v1_replacement(
            &PublicV1Scope {
                namespace: "default".into(),
                name: "sample".into(),
                resource_uid: ResourceUid::new("resource-1"),
            },
            &expected,
            &completion,
            &target_authority,
            &rebuild,
        )
        .await
        .unwrap();
        for _ in 0..4 {
            if !crate::reconciler::reconcile_unbound_replacement_scaffolding(
                &api.observation().await,
                api.as_ref(),
            )
            .await
            .unwrap()
            {
                break;
            }
        }
        let before_initialization = api.observation().await;
        let topology = before_initialization
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .unwrap()
            .configuration()
            .unwrap();
        let initialization = crate::reconciler::next_initialization_command(
            &before_initialization,
            &ResourceUid::new("resource-1"),
            &[ReplicaId::new(1)],
            Some(&topology),
        )
        .unwrap()
        .expect("fresh replacement initialization");
        api.execute_command(
            &before_initialization,
            &ProtocolCommand::InitializeAgentStore(Box::new(initialization.clone())),
        )
        .await
        .unwrap();
        let initialized = api.observation().await;
        let fresh = initialized
            .agents
            .values()
            .find_map(|observation| match observation {
                RawAgentObservation::PublicV1Report(report)
                    if report.replica.replica_id == ReplicaId::new(1)
                        && report.replica.instance_id != target.replica.instance_id =>
                {
                    Some(report.storage_incarnation.clone())
                }
                _ => None,
            })
            .expect("fresh initialized replacement");
        let replacement_operation = initialized
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_replacement.as_ref())
            .unwrap()
            .operation_id
            .clone();
        api.bind_public_v1_replacement(
            &PublicV1Scope {
                namespace: "default".into(),
                name: "sample".into(),
                resource_uid: ResourceUid::new("resource-1"),
            },
            &replacement_operation,
            &fresh,
        )
        .await
        .unwrap();
        let live_predecessor = api.observation().await;
        assert!(
            live_predecessor
                .pods
                .iter()
                .any(|pod| { pod.uid().as_deref() == Some(target.replica.instance_id.as_str()) })
        );
        assert!(
            api.effects()
                .await
                .iter()
                .any(|effect| matches!(effect, EffectRecord::EnsureReplacement(_))),
            "live predecessor did not allocate replacement: observation={:#?} effects={:#?}",
            api.observation().await,
            api.effects().await
        );
        assert_ne!(
            initialization.expected_pod_uid.as_str(),
            target.replica.instance_id.as_str()
        );
        assert_ne!(
            initialization.expected_pvc_uid,
            match &target.storage {
                ReplicaStorageIdentity::Persistent { pvc_uid, .. } => pvc_uid.clone(),
                ReplicaStorageIdentity::Volatile { .. } => unreachable!(),
            }
        );
        assert_eq!(
            initialization.expected_instance_id.as_str(),
            initialization.expected_pod_uid.as_str()
        );
        let replacement = live_predecessor
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_replacement.as_ref())
            .and_then(|replacement| replacement.replacement.as_ref())
            .expect("fresh replacement binding");
        assert_eq!(
            replacement.replica.instance_id.as_str(),
            initialization.expected_pod_uid.as_str()
        );
        assert_eq!(
            replacement.storage,
            ReplicaStorageIdentity::Persistent {
                pod_uid: initialization.expected_pod_uid,
                pvc_uid: initialization.expected_pvc_uid,
            }
        );
    }

    #[tokio::test]
    async fn ordinary_replacement_waits_for_exact_service_absence() {
        let predecessor = storage("predecessor");
        let target = storage("target");
        let configuration_id = ConfigurationId::new("configuration");
        let authority = PublicV1ReplacementState::new(&configuration_id, predecessor.clone())
            .unwrap()
            .bind(target.clone())
            .unwrap();
        let (expected, _, _, _) = corrective_inputs(&target);
        let mut observation = corrective_observation(authority, &expected, &target);
        let status = observation.set.status.as_mut().unwrap();
        status.public_v1_transition = None;
        status.public_v1_operation = None;
        observation
            .services
            .iter_mut()
            .find(|service| service.name_any() == "sample-write")
            .and_then(|service| service.spec.as_mut())
            .expect("write service")
            .selector = Some(BTreeMap::from([(
            INSTANCE_LABEL.into(),
            target.replica.instance_id.to_string(),
        )]));
        let ReplicaStorageIdentity::Persistent { pod_uid, .. } = &predecessor.storage else {
            unreachable!()
        };
        observation.services.push(Service {
            metadata: kube::core::ObjectMeta {
                name: Some("predecessor-service".into()),
                namespace: Some("default".into()),
                uid: Some("predecessor-service-uid".into()),
                ..Default::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(BTreeMap::from([(
                    INSTANCE_LABEL.into(),
                    pod_uid.to_string(),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        });
        let topology = observation
            .set
            .status
            .as_ref()
            .and_then(|status| status.public_v1_topology.as_ref())
            .unwrap()
            .configuration()
            .unwrap();
        assert!(!crate::reconciler::bootstrap_resources_missing(
            &observation,
            &[ReplicaId::new(1)],
            Some(&topology),
        ));
        let api = Arc::new(InMemoryClusterApi::new(observation));
        let scope = PublicV1Scope {
            namespace: "default".into(),
            name: "sample".into(),
            resource_uid: ResourceUid::new("resource-1"),
        };
        assert!(
            crate::reconciler::reconcile_replacement_cleanup(
                &api.observation().await,
                Some(&topology),
                &scope,
                api.as_ref(),
            )
            .await
            .unwrap()
        );
        let pending = api.observation().await;
        assert!(
            pending
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_replacement.as_ref())
                .is_some()
        );
        assert!(
            !pending.services.iter().any(|service| {
                service.spec.as_ref().is_some_and(|spec| {
                    spec.selector.as_ref().is_some_and(|selector| {
                        selector.get(INSTANCE_LABEL).map(String::as_str) == Some(pod_uid.as_str())
                    })
                })
            }),
            "predecessor service cleanup did not run: observation={pending:#?} effects={:#?}",
            api.effects().await
        );

        assert!(
            crate::reconciler::reconcile_replacement_cleanup(
                &api.observation().await,
                Some(&topology),
                &scope,
                api.as_ref(),
            )
            .await
            .unwrap()
        );
        assert!(
            api.observation()
                .await
                .set
                .status
                .as_ref()
                .and_then(|status| status.public_v1_replacement.as_ref())
                .is_none()
        );
    }
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
    use kuberic_runtime::protocol::public_operations::{
        ProcessSessionRenewalRequired, PublicProgress, PublicProgressRole,
        PublicSessionEpochBarrierStage, PublicV1Extensions, PublicV1Identity,
        PublicV1SessionRenewalStatus,
    };
    use kuberic_runtime::protocol::types::{
        AccessStatus, AgentGeneration, ConfigurationId, ConfigurationMember, Epoch,
        ProcessSessionId, ProcessSessionRenewalReason, PublicProtocolGeneration,
        ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ReplicatorCreationId,
        StorageGeneration, derive_bootstrap_history, derive_history_initialization_id,
    };

    fn renewal_endpoint(id: i64, session: &str, role: ReplicaRole) -> ProcessSessionEndpoint {
        let identity = ReplicaIdentity {
            replica_id: ReplicaId::new(id),
            instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
            agent_generation: AgentGeneration::new(format!("generation-{id}")),
        };
        ProcessSessionEndpoint {
            storage_incarnation: ReplicaStorageIncarnationContext {
                replica: identity.clone(),
                storage: ReplicaStorageIdentity::Persistent {
                    pod_uid: PodUid::new(format!("pod-{id}")),
                    pvc_uid: PvcUid::new(format!("pvc-{id}")),
                },
                storage_generation: StorageGeneration::new(format!("storage-{id}")),
            },
            identity,
            process_session_id: ProcessSessionId::new(session),
            replicator_creation_id: ReplicatorCreationId::new(format!("creation-{session}")),
            role,
        }
    }

    fn renewal_replica(
        identity: PublicV1Identity,
        history: &kuberic_runtime::protocol::types::HistoryContext,
        endpoint: &ProcessSessionEndpoint,
        report_role: ReplicaRole,
    ) -> PublicV1TopologyReplica {
        PublicV1TopologyReplica {
            report: PublicV1AgentReport {
                engine_idle: None,
                remote_builds: Vec::new(),
                retry_pending_operations: Vec::new(),
                active_operations: Vec::new(),
                configuration: None,
                identity,
                resource_uid: history.resource_uid.clone(),
                replica: endpoint.identity.clone(),
                process_session_id: endpoint.process_session_id.clone(),
                replicator_creation_id: endpoint.replicator_creation_id.clone(),
                report_sequence: 1,
                open_mode: kuberic_runtime::protocol::types::OpenMode::Existing,
                role: report_role,
                read_status: AccessStatus::NotPrimary,
                write_status: AccessStatus::NotPrimary,
                history: history.clone(),
                storage_incarnation: endpoint.storage_incarnation.clone(),
                progress: PublicProgress {
                    role: match report_role {
                        ReplicaRole::Primary => PublicProgressRole::Primary,
                        ReplicaRole::ActiveSecondary | ReplicaRole::IdleSecondary => {
                            PublicProgressRole::Secondary
                        }
                        ReplicaRole::None => PublicProgressRole::Recovery,
                    },
                    current_progress:
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0),
                    catch_up_capability:
                        kuberic_runtime::protocol::public_operations::ProgressQueryResult::Value(0),
                },
                replication_address: Some(format!(
                    "http://10.0.0.{}:50052",
                    endpoint.identity.replica_id,
                )),
                service_location: None,
                data_loss_recovery: None,
                completed_operations: Vec::new(),
                extensions: PublicV1Extensions::default(),
            },
            replication_address: format!("http://10.0.0.{}:50052", endpoint.identity.replica_id),
        }
    }

    #[test]
    fn unavailable_primary_is_observed_until_failover_delay_elapses() {
        let mut set = KubericSet::new(
            "sample",
            crate::crd::KubericSetSpec {
                replicas: 3,
                image: "example".into(),
                failover_delay_seconds: 10,
                switchover: None,
            },
        );
        set.status = Some(KubericSetStatus {
            public_v1_topology: Some(PublicV1TopologyState {
                configuration_id: ConfigurationId::new("configuration"),
                epoch: Epoch::new(0, 1),
                primary_id: ReplicaId::new(1),
                member_count: 3,
                sessions: Vec::new(),
                unavailable_primary_id: Some(ReplicaId::new(1)),
                unavailable_since_unix_seconds: Some(100),
                completed_switchover_request_id: None,
                pending_switchover: None,
                canonical_configuration_json: "{}".into(),
            }),
            ..Default::default()
        });

        assert!(should_observe_replica(&set, 109, ReplicaId::new(1)));
        assert!(!should_observe_replica(&set, 110, ReplicaId::new(1)));
        assert!(should_observe_replica(&set, 110, ReplicaId::new(2)));
        assert!(should_observe_replica(&set, 110, ReplicaId::new(3)));
    }

    fn renewal_configuration_fixture() -> (
        PublicV1AcceptedStatus,
        PublicV1SessionRenewalStatus,
        Vec<PublicV1TopologyReplica>,
    ) {
        let resource_uid = ResourceUid::new("resource-renewal-configuration");
        let generation = PublicProtocolGeneration::new(1);
        let initialization = derive_history_initialization_id(&resource_uid, generation);
        let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
        let primary = renewal_endpoint(1, "primary", ReplicaRole::Primary);
        let predecessor = renewal_endpoint(2, "secondary-old", ReplicaRole::ActiveSecondary);
        let mut replacement = predecessor.clone();
        replacement.process_session_id = ProcessSessionId::new("secondary-new");
        replacement.replicator_creation_id = ReplicatorCreationId::new("creation-secondary-new");
        replacement.role = ReplicaRole::None;
        let secondary = renewal_endpoint(3, "secondary-3", ReplicaRole::ActiveSecondary);
        let epoch = Epoch::new(0, 2);
        let configuration = ConfigurationDescriptor::new(
            epoch,
            primary.identity.replica_id,
            vec![
                ConfigurationMember {
                    identity: primary.identity.clone(),
                    role: ReplicaRole::Primary,
                },
                ConfigurationMember {
                    identity: replacement.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
                ConfigurationMember {
                    identity: secondary.identity.clone(),
                    role: ReplicaRole::ActiveSecondary,
                },
            ],
            2,
        );
        let required = ProcessSessionRenewalRequired {
            operation_id: OperationId::new("renewal-configuration"),
            reason: ProcessSessionRenewalReason::ActiveSecondaryReplacement,
            resource_uid,
            history: history.clone(),
            previous_epoch: Epoch::new(0, 1),
            renewal_epoch: epoch,
            predecessor,
            replacement: Some(replacement.clone()),
            surviving_sessions: vec![primary.clone(), secondary.clone()],
            fresh_configuration: configuration,
            predecessor_contained: true,
            data_loss: None,
        };
        let barriers = PublicV1SessionRenewalStatus::required(required)
            .unwrap()
            .close_access()
            .unwrap()
            .freeze_surviving_set()
            .unwrap()
            .begin_epoch_barriers()
            .unwrap();
        assert!(
            barriers
                .barriers
                .iter()
                .all(|barrier| { barrier.stage == PublicSessionEpochBarrierStage::Pending })
        );
        let barriers = barriers
            .record_epoch_updated(&primary, epoch)
            .unwrap()
            .record_epoch_updated(&secondary, epoch)
            .unwrap();
        let renewal = barriers
            .finish_epoch_barriers()
            .unwrap()
            .replay_replacement_role(
                &replacement.process_session_id,
                epoch,
                ReplicaRole::ActiveSecondary,
            )
            .unwrap();
        let authority = PublicV1AcceptedStatus {
            identity: PublicV1Identity::new(generation.value()),
            history_initialization_id: initialization,
            history: history.clone(),
            data_loss_recovery: None,
            completed_operations: Vec::new(),
            extensions: PublicV1Extensions::default(),
        };
        let replicas = vec![
            renewal_replica(
                authority.identity.clone(),
                &history,
                &primary,
                ReplicaRole::Primary,
            ),
            renewal_replica(
                authority.identity.clone(),
                &history,
                &replacement,
                ReplicaRole::None,
            ),
            renewal_replica(
                authority.identity.clone(),
                &history,
                &secondary,
                ReplicaRole::ActiveSecondary,
            ),
        ];

        (authority, renewal, replicas)
    }

    #[test]
    fn renewal_retry_reattaches_frozen_build_before_retiring_its_idle_slot() {
        use kuberic_runtime::protocol::public_operations::{
            ProgressQueryResult, PublicBuildInput, PublicOperationClass, PublicOperationIntent,
            PublicV1InstalledConfiguration,
        };
        let (authority, renewal, mut replicas) = renewal_configuration_fixture();
        let replacement = renewal.required.replacement.as_ref().unwrap();
        let retained = ConfigurationDescriptor::new(
            renewal.required.renewal_epoch,
            renewal.required.fresh_configuration.primary_id,
            renewal
                .required
                .fresh_configuration
                .members
                .iter()
                .filter(|member| member.identity != replacement.identity)
                .cloned()
                .collect(),
            2,
        );
        let voting = crate::evaluator::public_v1_topology::native_public_configuration(
            &replicas[0].report.replica,
            &replicas,
            &retained,
            None,
            None,
        )
        .unwrap();
        replicas[0].report.engine_idle = Some(
            kuberic_runtime::protocol::public_operations::PublicV1EngineIdleObservation {
                configuration: voting.clone(),
                history: replicas[0].report.history.clone(),
                storage_incarnation: replicas[0].report.storage_incarnation.clone(),
                process_session_id: replicas[0].report.process_session_id.clone(),
                replicator_creation_id: replicas[0].report.replicator_creation_id.clone(),
                idle_replicas: vec![replacement.identity.replica_id],
            },
        );
        replicas[0].report.configuration = Some(PublicV1InstalledConfiguration { voting });
        let pending = ProgressQueryResult::NotQueried("native copy is incomplete".into());
        replicas[1].report.progress.current_progress = pending.clone();
        replicas[1].report.progress.catch_up_capability = pending;
        let copy_id = OperationId::new(format!(
            "{}-native-copy-{}",
            renewal.required.operation_id, replacement.process_session_id,
        ));
        let program = PublicOperationProgram::Build(PublicBuildInput {
            attempt: copy_id.clone(),
            source_address: replicas[0].replication_address.clone(),
            epoch: retained.epoch,
            current_configuration_id: Some(retained.configuration_id.clone()),
            replica: replacement.identity.clone(),
            target_storage: replacement.storage_incarnation.clone(),
            process_session_id: replacement.process_session_id.clone(),
            replication_address: replicas[1].replication_address.clone(),
            history: Some(renewal.required.history.clone()),
        });
        let frozen = PublicOperationIntent {
            identity: authority.identity.clone(),
            operation_id: copy_id,
            revision: 7,
            process_session_id: replicas[0].report.process_session_id.clone(),
            class: PublicOperationClass::Build {
                target: replacement.identity.replica_id,
            },
            input_digest: program.digest(),
            replay_disposition: program.replay_disposition(),
            lifecycle: None,
            program: Some(program),
        };
        frozen.validate().unwrap();
        replicas[0].report.active_operations.push(frozen.clone());
        replicas[0].report.validate().unwrap();
        let Some(ProtocolCommand::PublicV1Operation(retry)) =
            renewal_native_command(&authority, &renewal, &replicas).unwrap()
        else {
            panic!("running copy must retain its exact operation");
        };
        assert_eq!(retry.target, replicas[0].report.replica);
        assert_eq!(retry.intent, frozen);
        replicas[0].report.active_operations.clear();
        let Some(ProtocolCommand::PublicV1Operation(retirement)) =
            renewal_native_command(&authority, &renewal, &replicas).unwrap()
        else {
            panic!("unowned predecessor idle slot must be retired before new copy");
        };
        assert_eq!(
            retirement.intent.program,
            Some(PublicOperationProgram::RetireIdleReplica {
                replica_id: replacement.identity.replica_id,
            })
        );
        let Some(PublicOperationProgram::Build(build)) = frozen.program.as_ref() else {
            panic!("fixture lost exact Build");
        };
        replicas[0].report.engine_idle = None;
        replicas[0].report.remote_builds.push(
            kuberic_runtime::protocol::public_operations::PublicV1RemoteBuildObservation {
                operation_id: frozen.operation_id.clone(),
                revision: frozen.revision,
                build: build.clone(),
                state:
                    kuberic_runtime::protocol::public_operations::PublicV1RemoteBuildState::InDrop,
            },
        );
        let completion =
            kuberic_runtime::protocol::public_operations::PublicV1OperationCompletion {
                operation_id: frozen.operation_id.clone(),
                revision: frozen.revision,
                call_kind: kuberic_runtime::protocol::public_operations::PublicV1CallKind::Build,
                process_session_id: frozen.process_session_id.clone(),
                input_digest: frozen.input_digest.clone(),
                replay_disposition: frozen.replay_disposition,
                history: renewal.required.history.clone(),
                storage_incarnation: replicas[0].report.storage_incarnation.clone(),
                previous_configuration_id: None,
                current_configuration_id: build.current_configuration_id.clone(),
                catch_up_mode: None,
                catch_up_specific_quorum: None,
                result: PublicV1OperationResult::Failed("contained predecessor Build".into()),
            };
        replicas[0].report.completed_operations.push(completion);
        replicas[0].report.validate().unwrap();
        let Some(ProtocolCommand::PublicV1Operation(retirement)) =
            renewal_native_command(&authority, &renewal, &replicas).unwrap()
        else {
            panic!("unavailable diagnostic must not hide known exact retirement");
        };
        assert_eq!(
            retirement.intent.program,
            Some(PublicOperationProgram::Remove(build.clone()))
        );
        assert_eq!(
            retirement.intent.class,
            PublicOperationClass::Remove {
                target: build.replica.replica_id
            }
        );
        assert_eq!(retirement.intent.revision, frozen.revision);
        assert!(
            renewal_completed_copy_revoke(&renewal, &replicas)
                .unwrap()
                .is_none()
        );
        // Simulate interruption after the retained Build succeeded but before
        // the old controller could issue revoke. A recreated controller has
        // only independently reobserved exact journal evidence, not its stack.
        replicas[0]
            .report
            .completed_operations
            .last_mut()
            .unwrap()
            .result = PublicV1OperationResult::Succeeded;
        replicas[0].report.remote_builds[0].state =
            kuberic_runtime::protocol::public_operations::PublicV1RemoteBuildState::Ready;
        replicas[0].report.validate().unwrap();
        let first_revoke = renewal_completed_copy_revoke(&renewal, &replicas)
            .unwrap()
            .unwrap();
        let mut reobserved = replicas.clone();
        for replica in &mut reobserved {
            replica.report =
                serde_json::from_slice(&serde_json::to_vec(&replica.report).unwrap()).unwrap();
        }
        assert_eq!(
            renewal_completed_copy_revoke(&renewal, &reobserved).unwrap(),
            Some(first_revoke.clone())
        );
        let ProtocolCommand::PublicV1History(command) = first_revoke else {
            panic!("expected exact revoke")
        };
        let kuberic_runtime::protocol::command::PublicV1HistoryCommand::RevokeInboundBuildAuthority { authority } = *command
            else { panic!("historical completion cannot reinstall a grant") };
        assert_eq!(authority.operation_id, frozen.operation_id);
        assert_eq!(authority.operation_revision, frozen.revision);
        assert_eq!(authority.attempt.target_session, build.process_session_id);
        assert_eq!(authority.attempt.target_storage, build.target_storage);
        assert_eq!(authority.attempt.source_session, frozen.process_session_id);
        assert_eq!(authority.source_address, build.source_address);
        assert_eq!(authority.target_address, build.replication_address);
        reobserved[1].report.process_session_id = ProcessSessionId::new("successor-session");
        assert!(renewal_completed_copy_revoke(&renewal, &reobserved).is_err());
    }

    #[test]
    fn renewal_configuration_uses_exact_sessions_and_configures_primary_last() {
        let (authority, renewal, replicas) = renewal_configuration_fixture();
        let commands = renewal_configuration_commands(&authority, &renewal, &replicas).unwrap();

        let targets = commands
            .iter()
            .map(|command| match command {
                ProtocolCommand::PublicV1Operation(operation) => operation.target.replica_id,
                _ => panic!("expected configuration operation"),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            targets,
            vec![
                replicas[1].report.replica.replica_id,
                replicas[2].report.replica.replica_id,
                replicas[0].report.replica.replica_id,
            ]
        );
        for command in commands {
            let ProtocolCommand::PublicV1Operation(operation) = command else {
                panic!("expected configuration operation");
            };
            let Some(PublicOperationProgram::Configuration(configuration)) =
                operation.intent.program.as_ref()
            else {
                panic!("expected configuration operation");
            };
            assert_eq!(configuration.replicas.len(), 2);
            assert!(configuration.replicas.iter().all(|replica| {
                replicas.iter().any(|expected| {
                    expected.report.replica == replica.identity
                        && expected.report.process_session_id == replica.process_session_id
                        && expected.replication_address == replica.replication_address
                })
            }));
        }
    }

    fn endpoint_observation(replicas: &[PublicV1TopologyReplica]) -> RawObservation {
        let mut set = KubericSet::new(
            "sample",
            crate::crd::KubericSetSpec {
                replicas: replicas.len() as u32,
                image: "example".into(),
                failover_delay_seconds: 10,
                switchover: None,
            },
        );
        set.metadata.uid = Some(replicas[0].report.resource_uid.to_string());
        set.metadata.namespace = Some("default".into());
        RawObservation {
            set,
            pods: replicas
                .iter()
                .map(|replica| {
                    serde_json::from_value(serde_json::json!({
                        "metadata": {
                            "name": format!("sample-{}", replica.report.replica.replica_id),
                            "uid": replica.report.replica.instance_id.as_str(),
                        },
                        "status": {
                            "podIP": format!("10.0.0.{}", replica.report.replica.replica_id),
                        },
                    }))
                    .unwrap()
                })
                .collect(),
            services: replicas
                .iter()
                .map(|replica| {
                    serde_json::from_value(serde_json::json!({
                        "spec": {
                            "selector": { INSTANCE_LABEL: replica.report.replica.instance_id.as_str() },
                            "ports": [{ "name": "replication", "port": REPLICATION_PORT }],
                        },
                    }))
                    .unwrap()
                })
                .collect(),
            agents: replicas
                .iter()
                .map(|replica| {
                    (
                        ReplicaObservationKey {
                            replica_id: replica.report.replica.replica_id,
                            instance_id: replica.report.replica.instance_id.clone(),
                        },
                        RawAgentObservation::PublicV1Report(Box::new(replica.report.clone())),
                    )
                })
                .collect(),
            pvcs: Vec::new(),
            secrets: Vec::new(),
            failures: Vec::new(),
            now_unix_seconds: 0,
        }
    }

    fn resolve_both_topology_paths(
        observation: &RawObservation,
        renewal: &PublicV1SessionRenewalStatus,
    ) -> [std::result::Result<Vec<String>, String>; 2] {
        let reports = observation
            .agents
            .values()
            .filter_map(|agent| match agent {
                RawAgentObservation::PublicV1Report(report) => Some(report.as_ref().clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        [
            crate::reconciler::topology_replicas(observation, None, &reports)
                .map_err(|error| error.to_string()),
            renewal_topology_replicas(observation, renewal),
        ]
        .map(|result| {
            result.map(|replicas| {
                replicas
                    .into_iter()
                    .map(|replica| replica.replication_address)
                    .collect()
            })
        })
    }

    #[test]
    fn replication_endpoint_uses_open_result_in_both_topology_paths() {
        let (_, renewal, replicas) = renewal_configuration_fixture();
        let mut observation = endpoint_observation(&replicas);
        let expected = observation
            .agents
            .values_mut()
            .enumerate()
            .map(|(index, agent)| {
                let RawAgentObservation::PublicV1Report(report) = agent else {
                    unreachable!();
                };
                let address = format!("http://127.0.0.1:{}", 41001 + index);
                report.replication_address = Some(address.clone());
                address
            })
            .collect::<Vec<_>>();
        for result in resolve_both_topology_paths(&observation, &renewal) {
            assert_eq!(result.unwrap(), expected);
        }
        observation.pods[0].status.as_mut().unwrap().pod_ip = Some("::1".into());
        for result in resolve_both_topology_paths(&observation, &renewal) {
            assert_eq!(result.unwrap(), expected);
        }
    }

    #[test]
    fn replication_endpoint_absence_allows_open_but_blocks_renewal_configuration() {
        let (_, renewal, replicas) = renewal_configuration_fixture();
        let mut observation = endpoint_observation(&replicas);
        for agent in observation.agents.values_mut() {
            let RawAgentObservation::PublicV1Report(report) = agent else {
                unreachable!();
            };
            report.replication_address = None;
        }
        let [ordinary, renewal] = resolve_both_topology_paths(&observation, &renewal);
        assert_eq!(ordinary.unwrap(), vec![String::new(); replicas.len()]);
        assert!(renewal.unwrap_err().contains("Replicator Open endpoint"));
    }

    #[test]
    fn replication_endpoint_discovery_preserves_pod_service_and_session_validation() {
        let (_, renewal, replicas) = renewal_configuration_fixture();
        let observation = endpoint_observation(&replicas);
        for invalid in 0..5 {
            let mut stale = observation.clone();
            match invalid {
                0 => stale.pods[0].metadata.uid = Some("replacement-pod".into()),
                1 => stale.services[0].spec.as_mut().unwrap().selector = None,
                2 => stale.services[0].spec.as_mut().unwrap().ports = None,
                3 => stale.pods[0].status.as_mut().unwrap().pod_ip = None,
                4 => {
                    stale.pods[0].status.as_mut().unwrap().pod_ip = Some("not-an-ip".into());
                }
                _ => unreachable!(),
            }
            for result in resolve_both_topology_paths(&stale, &renewal) {
                assert!(result.is_err());
            }
        }
        for invalid in 0..3 {
            let mut stale = observation.clone();
            let RawAgentObservation::PublicV1Report(report) =
                stale.agents.values_mut().next().unwrap()
            else {
                unreachable!();
            };
            match invalid {
                0 => report.process_session_id = ProcessSessionId::new("stale-session"),
                1 => report.replicator_creation_id = ReplicatorCreationId::new("stale-creation"),
                2 => {
                    report.storage_incarnation.storage_generation =
                        StorageGeneration::new("stale-storage");
                }
                _ => unreachable!(),
            }
            assert!(renewal_topology_replicas(&stale, &renewal).is_err());
        }
    }

    #[test]
    fn initialization_execute_accepts_only_the_exact_fresh_observation() {
        let resource_uid = ResourceUid::new("resource-init-response");
        let replica_id = ReplicaId::new(2);
        let pod_uid = PodUid::new("pod-init-response");
        let pvc_uid = PvcUid::new("pvc-init-response");
        let initialization_id =
            derive_initialization_id(&resource_uid, replica_id, &pod_uid, &pvc_uid);
        let identity = ReplicaIdentity {
            replica_id,
            instance_id: ReplicaInstanceId::new(pod_uid.as_str()),
            agent_generation: derive_agent_generation(&initialization_id),
        };
        let command = ProtocolCommand::InitializeAgentStore(Box::new(InitializeAgentStore {
            initialization_id,
            resource_uid: resource_uid.clone(),
            local_replica_id: replica_id,
            expected_instance_id: identity.instance_id,
            expected_pod_uid: pod_uid.clone(),
            expected_pvc_uid: pvc_uid.clone(),
            assigned_agent_generation: identity.agent_generation,
            effective_policy: EffectivePolicy::fixed(3, 30).unwrap(),
            bootstrap_configuration: ConfigurationDescriptor::new(
                Epoch::new(0, 1),
                ReplicaId::new(1),
                vec![ConfigurationMember {
                    identity: ReplicaIdentity {
                        replica_id: ReplicaId::new(1),
                        instance_id: ReplicaInstanceId::new("primary-pod"),
                        agent_generation: kuberic_runtime::protocol::types::AgentGeneration::new(
                            "primary-generation",
                        ),
                    },
                    role: ReplicaRole::Primary,
                }],
                1,
            ),
            provisioning: None,
        }));
        let exact = proto::AgentStatusReport {
            protocol_version:
                kuberic_runtime::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION,
            resource_uid: resource_uid.to_string(),
            process_session_id: "fresh-process".into(),
            storage_state: proto::AgentStorageState::Uninitialized as i32,
            pod_uid: pod_uid.to_string(),
            pvc_uid: pvc_uid.to_string(),
            healthy: true,
            replica_id: replica_id.value(),
            ..Default::default()
        };
        validate_execute_observation(&command, exact.clone()).unwrap();

        let mut wrong = exact;
        wrong.pvc_uid = "other-pvc".into();
        assert!(validate_execute_observation(&command, wrong).is_err());
    }

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
