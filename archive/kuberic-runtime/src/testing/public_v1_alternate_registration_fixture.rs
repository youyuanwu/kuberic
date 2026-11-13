use crate::replicator::{DefaultReplicatorFactory, PublicV1FactoryRegistration};

pub(super) fn construct_alternate_builtin_factory() -> DefaultReplicatorFactory {
    DefaultReplicatorFactory::new()
}

pub(super) fn alternate_builtin_factory_registration() -> PublicV1FactoryRegistration {
    PublicV1FactoryRegistration::alternate(
        "unmanifested built-in factory construction",
        construct_alternate_builtin_factory,
    )
}
