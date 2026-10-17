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
use kuberic_runtime::protocol::types::{Epoch, OperationId, ReplicaId, ReplicaRole};
use kuberic_runtime::replicator::{
    PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode,
    Replicator, ReplicatorCapabilities, ReplicatorInterfaces,
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
