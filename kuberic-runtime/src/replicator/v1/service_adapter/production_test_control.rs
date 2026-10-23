//! Actual production effects assembled with test-owned facade/transport owners.

use super::*;

pub(crate) fn test_control(
    context: ReplicatorFactoryContext,
    facade: Arc<Facade>,
    transport: Arc<Transport>,
) -> Arc<dyn NativeRuntimeControl> {
    let owner = Arc::new(Production {
        settings_address: transport.local().address.clone(),
        facade,
        transport,
        context: context.clone(),
        peers: Arc::new(Mutex::new(BTreeMap::new())),
        epoch: Mutex::new(None),
    });
    Arc::new(crate::replicator::coordinator::Control::new(context, owner))
}
