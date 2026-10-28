use std::net::SocketAddr;
use std::path::PathBuf;

use kuberic_runtime::protocol::public_operations::{
    PublicV1Identity, PublicV1LifecycleBinding, StatePersistence,
};
use kuberic_runtime::protocol::types::{
    HistoryContext, HistoryInitializationId, InitializationId, OpenMode, PodUid, ProcessSessionId,
    PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId,
    ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ReplicatorCreationId, ResourceUid,
    StorageGeneration, derive_agent_generation, derive_bootstrap_history,
    derive_history_initialization_id, derive_initialization_id,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const TOKEN_ENV: &str = "ORDINARY21_OWNER_TOKEN";
pub const MAX_FRAME_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct StableReplica {
    pub resource_uid: ResourceUid,
    pub replica_id: ReplicaId,
    pub pod_uid: PodUid,
    pub pvc_uid: PvcUid,
    pub generation: u64,
    pub spec_generation: u64,
}

impl StableReplica {
    pub fn initialization_id(&self) -> InitializationId {
        derive_initialization_id(
            &self.resource_uid,
            self.replica_id,
            &self.pod_uid,
            &self.pvc_uid,
        )
    }

    pub fn identity(&self) -> ReplicaIdentity {
        ReplicaIdentity {
            replica_id: self.replica_id,
            instance_id: ReplicaInstanceId::new(self.pod_uid.as_str()),
            agent_generation: derive_agent_generation(&self.initialization_id()),
        }
    }

    pub fn history_initialization_id(&self) -> HistoryInitializationId {
        derive_history_initialization_id(
            &self.resource_uid,
            PublicProtocolGeneration::new(self.generation),
        )
    }

    pub fn bootstrap_history(&self) -> HistoryContext {
        derive_bootstrap_history(
            &self.resource_uid,
            PublicProtocolGeneration::new(self.generation),
            &self.history_initialization_id(),
        )
    }

    pub fn initial_storage(&self) -> ReplicaStorageIncarnationContext {
        ReplicaStorageIncarnationContext {
            replica: self.identity(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: self.pod_uid.clone(),
                pvc_uid: self.pvc_uid.clone(),
            },
            storage_generation: StorageGeneration::new(format!(
                "storage-{}",
                self.initialization_id()
            )),
        }
    }

    pub fn binding(&self) -> PublicV1LifecycleBinding {
        PublicV1LifecycleBinding {
            identity: PublicV1Identity::new(self.generation),
            resource_uid: self.resource_uid.clone(),
            spec_generation: self.spec_generation,
            state_persistence: StatePersistence::Persisted,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeerEndpoint {
    pub identity: ReplicaIdentity,
    pub control_endpoint: String,
    pub replication_endpoint: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildReady {
    pub process_id: u32,
    pub root: PathBuf,
    pub agent_root: PathBuf,
    pub application_root: PathBuf,
    pub stable: StableReplica,
    pub identity: ReplicaIdentity,
    pub initialization_id: InitializationId,
    pub expected_bootstrap_history: HistoryContext,
    pub expected_initial_storage: ReplicaStorageIncarnationContext,
    pub control_endpoint: String,
    pub data_endpoint: SocketAddr,
    pub requested_replication_bind: SocketAddr,
    pub application_was_empty: bool,
    pub hold_first_open: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ChildEvent {
    IpcReady { ready: ChildReady },
    HostStarted {
        process_id: u32,
        process_session_id: ProcessSessionId,
    },
    OpenHeld {
        process_id: u32,
        identity: ReplicaIdentity,
        mode: OpenMode,
        process_session_id: Option<ProcessSessionId>,
        replicator_creation_id: Option<ReplicatorCreationId>,
    },
    BoundFactoryOverridesRejected {
        process_id: u32,
        callbacks: usize,
        variants: Vec<String>,
    },
    IpcError { message: String },
    Stopped { process_id: u32, failures: Vec<String> },
    Failed { message: String },
}

// Intentionally no Debug implementation: this envelope carries a credential.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParentRequest {
    pub request_id: u64,
    pub bearer_token: String,
    pub command: ChildCommand,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "camelCase", deny_unknown_fields)]
pub enum ChildCommand {
    Set { value: i64 },
    ServingRead,
    CommittedValueDiagnostic,
    ReleaseOpen,
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "camelCase")]
pub enum ChildReply {
    SetAcknowledged { lsn: i64 },
    ServingRead { value: i64 },
    CommittedValueDiagnostic { value: i64 },
    OpenReleased { released: bool },
    ShutdownRequested,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChildError {
    pub kind: ChildErrorKind,
    pub message: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ChildErrorKind {
    Unauthorized,
    InvalidState,
    NotOpen,
    NotPrimary,
    ReadClosed,
    WriteClosed,
    Closed,
    OperationCancelled,
    Runtime,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ParentReply {
    pub request_id: u64,
    pub result: Result<ChildReply, ChildError>,
}

#[allow(dead_code)]
pub fn authorized<T>(message: T, token: &str) -> anyhow::Result<tonic::Request<T>> {
    let mut request = tonic::Request::new(message);
    let authorization = format!("Bearer {token}")
        .parse()
        .map_err(|_| anyhow::anyhow!("credential is not valid authorization metadata"))?;
    request.metadata_mut().insert("authorization", authorization);
    Ok(request)
}

pub async fn read_frame<T: DeserializeOwned>(
    reader: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<T> {
    let length = reader.read_u32().await? as usize;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid fixture frame length",
        ));
    }
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid fixture JSON frame")
    })
}

pub async fn write_frame<T: Serialize>(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &T,
) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|_| std::io::Error::other("fixture JSON serialization failed"))?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid fixture frame length",
        ));
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.flush().await
}
