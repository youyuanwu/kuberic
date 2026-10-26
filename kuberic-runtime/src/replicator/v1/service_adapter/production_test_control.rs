//! Actual production effects assembled with test-owned facade/transport owners.

use super::*;

impl Production {
    #[cfg(feature = "testing")]
    pub(in crate::replicator::v1::service_adapter) fn removal_primary(
        &self,
    ) -> crate::replicator::v1::primary::PrimaryHandle {
        match self.facade.service().target().unwrap() {
            super::super::Target::Primary(primary) => primary,
            _ => panic!("actual primary required"),
        }
    }

    #[cfg(test)]
    pub(in crate::replicator::v1::service_adapter) fn drainage_secondary(
        &self,
    ) -> crate::replicator::v1::secondary::SecondaryHandle {
        match self.facade.service().target().unwrap() {
            super::super::Target::Secondary(secondary) => secondary,
            _ => panic!("actual secondary required"),
        }
    }
}

pub(crate) async fn test_compose(
    context: ReplicatorFactoryContext,
    provider: Arc<dyn StateProvider>,
    registry: MessageRegistry,
    transport: Arc<Transport>,
) -> Result<ReplicatorInterfaces> {
    compose(context, provider, registry, transport).await
}

pub(crate) fn test_provider(
    provider: Arc<dyn StateProvider>,
    transport: Arc<Transport>,
) -> Arc<dyn StateProvider> {
    Arc::new(SessionProvider {
        provider,
        transport,
    })
}

pub(crate) fn test_control(
    context: ReplicatorFactoryContext,
    facade: Arc<Facade>,
    transport: Arc<Transport>,
) -> Arc<dyn Replicator> {
    Arc::new(Production {
        settings_address: transport.local().address.clone(),
        facade,
        transport,
        context: context.clone(),
        peers: Arc::new(Mutex::new(BTreeMap::new())),
        epoch: Mutex::new(None),
        close: Mutex::new(None),
        builds: Mutex::new(BTreeMap::new()),
        recovery_tasks: tokio_util::task::TaskTracker::new(),
        recover_on_epoch: std::sync::atomic::AtomicBool::new(false),
        role_transition_pending: std::sync::atomic::AtomicBool::new(false),
        role_changing: Arc::new(AtomicBool::new(false)),
        #[cfg(feature = "testing")]
        recovery_spawned: std::sync::atomic::AtomicUsize::new(0),
    })
}
