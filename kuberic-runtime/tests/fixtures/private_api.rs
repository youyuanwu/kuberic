#![forbid(unsafe_code)]

use std::sync::Arc;

use kuberic_runtime::capabilities::{ReplicatorCreationIdentity, RuntimeHostToken};
use kuberic_runtime::host::{
    command, hosting, native_transport, operation, operation_recovery, provisioning,
    public_build_store, public_lifecycle, service, session, sqlite_store, state, store, transport,
};
use kuberic_runtime::replicator::stream::{OperationKind, OperationStream, ReceivedOperation};
use kuberic_runtime::replicator::{
    NativeRuntimeControl, PartitionAccessView, Replicator, ReplicatorAttachment,
    ReplicatorCreationReservation, ReplicatorFactoryContext, ReplicatorInterfaces,
    ReplicatorRegistration, StatefulServicePartition, v1,
};
fn forge_partition(context: ReplicatorFactoryContext) {
    let _ = StatefulServicePartition::new(Default::default(), Default::default(), context);
}

fn forge_factory_context() {
    let _ = ReplicatorFactoryContext::new(
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
        Default::default(),
    );
}

fn forge_interfaces(control: Arc<dyn Replicator>) {
    let _ = ReplicatorInterfaces {
        replicator: control,
        state_replicator: None,
        primary_replicator: None,
    };
}

fn forge_received_operation() {
    let _ = ReceivedOperation::new(
        OperationKind::Replication,
        1,
        Default::default(),
        Default::default(),
        false,
        Arc::new(|| Ok(())),
    );
}

fn forge_operation_stream() {
    let _ = OperationStream::new(Box::pin(futures::stream::empty()), Arc::new(|_| Ok(())));
}

fn extract_native_controls(replicator: &dyn Replicator) {
    let _ = replicator.selected_epoch();
    let _ = replicator.idle_replicas();
    let _ = replicator.containment();
}

fn main() {}
