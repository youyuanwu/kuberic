use std::sync::Arc;

use kuberic_runtime::replicator::{
    PrimaryReplicator, Replicator, ReplicatorFactoryContext, ReplicatorInterfaces,
    ReplicatorRegistration, StateReplicator, StatefulServicePartition,
};

pub fn obtain_managed(interfaces: &ReplicatorInterfaces) {
    let _ = interfaces.managed_replicator();
}

pub fn infer_host_token(
    registration: Arc<dyn ReplicatorRegistration>,
    context: ReplicatorFactoryContext,
) {
    let _ = StatefulServicePartition::new(Default::default(), registration, context);
}

pub fn assemble_split_bundle(
    control: Arc<dyn Replicator>,
    state: Arc<dyn StateReplicator>,
    primary: Arc<dyn PrimaryReplicator>,
) {
    let _ = ReplicatorInterfaces::new(control, state, Some(primary));
}
