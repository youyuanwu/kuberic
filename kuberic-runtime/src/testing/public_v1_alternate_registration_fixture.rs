use std::sync::Arc;

use crate::engine::DurableState;
use crate::replicator::PublicV1FactoryRegistration;
use crate::replicator::stateless::StatelessReplicatorFactory as AlternateFactory;

pub(super) fn construct_alternate_builtin_factory(
    storage: Arc<dyn DurableState>,
) -> AlternateFactory {
    AlternateFactory::new(storage)
}

pub(super) fn alternate_builtin_factory_registration() -> PublicV1FactoryRegistration {
    PublicV1FactoryRegistration::stateless(
        "unmanifested built-in factory construction",
        construct_alternate_builtin_factory,
    )
}
