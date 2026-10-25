#![forbid(unsafe_code)]

use std::sync::Arc;

use kuberic_runtime::capabilities::{ReplicatorCreationIdentity, RuntimeHostToken};
use kuberic_runtime::host::{
    command, hosting, native_transport, operation, operation_recovery, provisioning,
    public_build_store, public_lifecycle, service, session, sqlite_store, state, store, transport,
};
use kuberic_runtime::replicator::{
    PartitionAccessView, Replicator, ReplicatorAttachment, ReplicatorCreation,
    ReplicatorCreationReservation, ReplicatorFactoryContext, ReplicatorInterfaces,
    ReplicatorRegistration, StatefulServicePartition, v1,
};
use kuberic_runtime::testing::without_catch_up_specific_quorum;

pub fn extract_attachment(
    interfaces: &ReplicatorInterfaces,
    reservation: ReplicatorCreationReservation,
) {
    let _ = interfaces.prepare_attachment(reservation);
}

pub fn infer_host_token(
    registration: Arc<dyn ReplicatorRegistration>,
    context: ReplicatorFactoryContext,
) {
    let _ = StatefulServicePartition::new(Default::default(), registration, context);
}

pub fn assemble_split_bundle(control: Arc<dyn Replicator>) {
    let _ = ReplicatorInterfaces {
        replicator: control,
        state_replicator: None,
        primary_replicator: None,
    };
}

pub fn disarm_bundle_guard(attachment: &ReplicatorAttachment) {
    attachment.disarm();
}

pub fn forge_access(_: Arc<dyn PartitionAccessView>) {}
pub fn forge_identity(_: ReplicatorCreationIdentity, _: RuntimeHostToken) {}
