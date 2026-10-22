#![forbid(unsafe_code)]

use std::{pin::Pin, sync::Arc};

use async_trait::async_trait;
use bytes::Bytes;
use futures::Stream;
use kuberic_runtime::Result;
use kuberic_runtime::application::{
    ClientWrite, Lsn, OpenMode, OperationData, OperationDataStream, StateProvider,
};
use kuberic_runtime::host::{
    ApplicationStorageState, KubernetesDnsResolver, ReplicaDiagnostics, ReplicaEndpointResolver,
    ReplicaHandle, ReplicaHost, ReplicaProcessConfig, RunningReplica,
};
use kuberic_runtime::protocol::public_operations::{
    ProgressQueryResult, PublicBuildInput, PublicConfiguration, PublicInboundBuildAuthority,
    PublicOperationIntent, PublicOperationProgram, PublicOperationReplayDisposition,
    PublicProgress, PublicProgressRole, PublicV1AgentReport, PublicV1InstalledConfiguration,
};
use kuberic_runtime::protocol::types::{Epoch, OperationId, ReplicaId, ReplicaRole};
use kuberic_runtime::replicator::stream::{OperationKind, OperationStream, ReceivedOperation};
use kuberic_runtime::replicator::{
    DefaultReplicatorFactory, PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration,
    ReplicaSetQuorumMode, Replication, Replicator, ReplicatorCapabilities, ReplicatorInterfaces,
    ReplicatorSettings, StateReplicator,
    CustomReplicatorControl, ReplicatorFactoryContext,
};

// These external identities deliberately expand aliases in both directions.
// Implementing the entire protected trio also pins their concrete method types.
fn bytes_identity(data: OperationData, concrete: Bytes) -> (Bytes, OperationData) {
    (data, concrete)
}

fn lsn_identity(lsn: Lsn, concrete: i64) -> (i64, Lsn) {
    (lsn, concrete)
}

fn stream_to_concrete(
    stream: OperationDataStream,
) -> Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>> {
    stream
}

fn stream_from_concrete(
    stream: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>,
) -> OperationDataStream {
    stream
}

struct ProtectedContracts;

// Compile-only independent companion: not native/database qualification.
#[async_trait]
impl CustomReplicatorControl for ProtectedContracts {
    async fn install_inbound_build_authority(&self, _: &PublicInboundBuildAuthority) -> Result<()> {
        Err(kuberic_runtime::RuntimeError::AuthorityNotAdmitted)
    }
    async fn revoke_inbound_build_authority(&self, _: &PublicInboundBuildAuthority) -> Result<()> {
        Err(kuberic_runtime::RuntimeError::AuthorityNotAdmitted)
    }
    async fn idle_replicas(&self, _: &PublicConfiguration) -> Result<Vec<ReplicaId>> {
        Err(kuberic_runtime::RuntimeError::ProgressNotReady)
    }
    async fn shutdown_and_drain(&self) -> Result<()> { Ok(()) }
}

async fn custom_controller_api(context: ReplicatorFactoryContext, build: OperationId) -> Result<()> {
    let owner = Arc::new(ProtectedContracts);
    let _bundle = ReplicatorInterfaces::primary(
        owner.clone(), None, ReplicatorCapabilities::conservative(),
    ).with_custom_control(&context, owner)?;
    let authority = context.outbound_build_authority(&build).await?;
    context.validate_build_authority(&authority).await
}

#[async_trait]
impl Replicator for ProtectedContracts {
    async fn open(&self) -> Result<String> {
        Ok(String::new())
    }
    async fn change_role(&self, _: Epoch, _: ReplicaRole) -> Result<()> {
        Ok(())
    }
    async fn update_epoch(&self, _: Epoch) -> Result<()> {
        Ok(())
    }
    async fn close(&self) -> Result<()> {
        Ok(())
    }
    fn abort(&self) {}
    async fn current_progress(&self) -> Result<i64> {
        Ok(0)
    }
    async fn catch_up_capability(&self) -> Result<i64> {
        Ok(0)
    }
}

#[async_trait]
impl PrimaryReplicator for ProtectedContracts {
    async fn on_data_loss(&self) -> Result<bool> {
        Ok(false)
    }
    async fn update_catch_up_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
        _: ReplicaSetConfiguration,
    ) -> Result<()> {
        Ok(())
    }
    async fn wait_for_catch_up_quorum(&self, _: ReplicaSetQuorumMode) -> Result<()> {
        Ok(())
    }
    async fn update_current_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
    ) -> Result<()> {
        Ok(())
    }
    async fn build_replica(&self, _: ReplicaInformation) -> Result<()> {
        Ok(())
    }
    async fn remove_replica(&self, _: ReplicaId) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl StateProvider for ProtectedContracts {
    async fn update_epoch(&self, _: Epoch, _: i64) -> Result<()> {
        Ok(())
    }
    async fn last_committed_lsn(&self) -> Result<i64> {
        Ok(0)
    }
    async fn get_copy_context(&self) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>> {
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn get_copy_state(
        &self,
        _: i64,
        context: Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>,
    ) -> Result<Pin<Box<dyn Stream<Item = Result<Bytes>> + Send>>> {
        Ok(context)
    }
    async fn on_data_loss(&self) -> Result<bool> {
        Ok(false)
    }
}

#[async_trait]
impl StateReplicator for ProtectedContracts {
    fn begin_replicate(&self, _: OperationData) -> Result<Replication> {
        Ok(Replication {
            lsn: 1,
            completion: Box::pin(async { Ok(()) }),
        })
    }

    fn replication_operations(&self) -> Result<OperationStream> {
        Err(kuberic_runtime::RuntimeError::Application(
            "no receiver".into(),
        ))
    }

    fn copy_operations(&self) -> Result<OperationStream> {
        Err(kuberic_runtime::RuntimeError::Application(
            "no receiver".into(),
        ))
    }

    async fn update_replicator_settings(&self, _: ReplicatorSettings) -> Result<()> {
        Ok(())
    }
}

async fn native_replication_api(state: &dyn StateReplicator) -> Result<Lsn> {
    let Replication { lsn, completion } = state.begin_replicate(Bytes::new())?;
    let _: Lsn = lsn;
    completion.await?;
    let _: Lsn = state.replicate(Bytes::new()).await?;
    let _ = DefaultReplicatorFactory::new();
    let mut streams = [state.replication_operations()?, state.copy_operations()?];
    for stream in &mut streams {
        if let Some(operation) = stream.get_operation().await? {
            let _: &ReceivedOperation = &operation;
            let _: OperationKind = operation.kind;
            let _: Lsn = operation.lsn;
            let _: Epoch = operation.epoch;
            let _: &Bytes = &operation.data;
            let _: bool = operation.end_of_stream;
            operation.acknowledge()?;
        }
    }
    Ok(lsn)
}

fn independent_progress_api() -> PublicProgress {
    let outcomes = [
        ProgressQueryResult::Value(7),
        ProgressQueryResult::NotQueried("no role".into()),
        ProgressQueryResult::NotReady,
        ProgressQueryResult::Failed("provider failed".into()),
    ];
    let _ = outcomes;
    PublicProgress {
        role: PublicProgressRole::Primary,
        current_progress: ProgressQueryResult::Value(7),
        catch_up_capability: ProgressQueryResult::Value(11),
    }
}

fn installed_configuration_api(
    report: &mut PublicV1AgentReport,
    voting: PublicConfiguration,
    idle_replicas: Vec<ReplicaId>,
    wire: &mut kuberic_runtime::control::proto::PublicV1AgentStatus,
) {
    let installed = PublicV1InstalledConfiguration {
        voting,
        idle_replicas,
    };
    report.configuration = Some(installed);
    let _: &Option<PublicV1InstalledConfiguration> = &report.configuration;
    let _: &Option<String> = &wire.configuration_json;
    wire.configuration_json = None;
}

fn active_control_observers_api(
    report: &mut PublicV1AgentReport,
    active_operations: Vec<PublicOperationIntent>,
    wire: &mut kuberic_runtime::control::proto::PublicV1AgentStatus,
) {
    report.active_operations = active_operations;
    let _: &Vec<PublicOperationIntent> = &report.active_operations;
    let _: &Vec<String> = &wire.active_operations_json;
    wire.active_operations_json = Vec::new();
}

fn frozen_build_endpoints_api(
    build: &PublicBuildInput,
    authority: &mut PublicInboundBuildAuthority,
) {
    let _: &String = &build.source_address;
    authority.source_address = build.source_address.clone();
    authority.target_address = build.replication_address.clone();
    let _: &String = &authority.source_address;
}

fn conservative_native_creation_api(
    interfaces: ReplicatorInterfaces,
    primary: Arc<ProtectedContracts>,
) -> Result<ReplicatorInterfaces> {
    let state = interfaces.state_replicator();
    let wrapped = interfaces.wrap_primary(primary, state);
    kuberic_runtime::testing::without_catch_up_specific_quorum(wrapped)
}

async fn source_idle_retirement_api(
    primary: &dyn PrimaryReplicator,
    replica_id: ReplicaId,
) -> Result<PublicOperationProgram> {
    let retirement = PublicOperationProgram::RetireIdleReplica { replica_id };
    let _: PublicOperationReplayDisposition = retirement.replay_disposition();
    primary.remove_replica(replica_id).await?;
    Ok(retirement)
}

fn supported_trait_reexports(
    provider: &dyn kuberic_runtime::StateProvider,
    service: &dyn kuberic_runtime::StatefulServiceReplica,
    control: Arc<dyn kuberic_runtime::Replicator>,
    primary: Arc<dyn kuberic_runtime::PrimaryReplicator>,
    state: Arc<dyn kuberic_runtime::StateReplicator>,
    factory: Arc<dyn kuberic_runtime::ReplicatorFactory>,
) {
    let _: &dyn kuberic_runtime::application::StateProvider = provider;
    let _: &dyn kuberic_runtime::application::StatefulServiceReplica = service;
    let _: Arc<dyn Replicator> = control;
    let _: Arc<dyn PrimaryReplicator> = primary;
    let _: Arc<dyn kuberic_runtime::replicator::StateReplicator> = state;
    let _: Arc<dyn kuberic_runtime::replicator::ReplicatorFactory> = factory;
}

fn ordinary_application(control: Arc<dyn Replicator>, primary: Arc<dyn PrimaryReplicator>) {
    let _ = ReplicatorInterfaces::secondary(control, None, ReplicatorCapabilities::conservative());
    let _ = primary.change_role(Epoch::new(1, 2), ReplicaRole::Primary);
    let _ = ClientWrite {
        operation_id: OperationId::new("external-write"),
        data: Default::default(),
    };
    let _ = OpenMode::Existing;
}

fn host_application<A, R>(
    config: ReplicaProcessConfig,
    control_listener: tokio::net::TcpListener,
    application: Arc<A>,
    resolver: Arc<R>,
) where
    A: kuberic_runtime::StatefulServiceReplica + 'static,
    R: ReplicaEndpointResolver + 'static,
{
    let host = ReplicaHost::new(
        config,
        application,
        ApplicationStorageState::Established,
        resolver,
    )
    .with_application_storage_paths(Default::default());
    let _ = host.start(control_listener);
    let _ = KubernetesDnsResolver::new(
        kuberic_runtime::protocol::types::ResourceUid::new("resource"),
        "namespace",
    );
    let _ = <KubernetesDnsResolver as ReplicaEndpointResolver>::control_endpoint;
}

async fn observe_host(replica: &mut RunningReplica, handle: ReplicaHandle) {
    let _: ReplicaHandle = replica.handle();
    let _ = replica.shutdown_signal();
    let _: kuberic_runtime::host::Result<ReplicaDiagnostics> = handle.diagnostics().await;
    replica.shutdown();
    let _: kuberic_runtime::host::Result<()> = replica.wait().await;
}

fn main() {}
