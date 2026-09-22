use std::sync::Arc;

use kuberic_runtime::replicator::{
    PrimaryReplicator, Replicator, ReplicatorInterfaces, StateReplicator,
};

pub fn obtain_managed(interfaces: &ReplicatorInterfaces) {
    let _ = interfaces.managed_replicator();
}

pub fn assemble_split_bundle(
    control: Arc<dyn Replicator>,
    state: Arc<dyn StateReplicator>,
    primary: Arc<dyn PrimaryReplicator>,
) {
    let _ = ReplicatorInterfaces::new(control, state, Some(primary));
}
