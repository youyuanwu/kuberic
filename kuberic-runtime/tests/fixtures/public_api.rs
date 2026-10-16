#![forbid(unsafe_code)]

use std::sync::Arc;

use kuberic_runtime::application::{ClientWrite, OpenMode};
use kuberic_runtime::host::{
    ApplicationStorageState, KubernetesDnsResolver, ReplicaDiagnostics, ReplicaEndpointResolver,
    ReplicaHandle, ReplicaHost, ReplicaProcessConfig, RunningReplica,
};
use kuberic_runtime::protocol::types::{Epoch, OperationId, ReplicaRole};
use kuberic_runtime::replicator::{
    PrimaryReplicator, Replicator, ReplicatorCapabilities, ReplicatorInterfaces,
};

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
    let _ = ReplicatorInterfaces::secondary(
        control,
        None,
        ReplicatorCapabilities::conservative(),
    );
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
)
where
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
