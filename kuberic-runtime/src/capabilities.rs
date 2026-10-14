use crate::protocol::types::ReplicatorCreationId;

#[derive(Debug, Clone, Copy)]
pub(crate) struct RuntimeHostToken {
    _private: (),
}

impl RuntimeHostToken {
    pub(crate) fn new() -> Self {
        Self { _private: () }
    }
}

/// Identity of a reserved replicator creation and its coherent capability bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ReplicatorCreationIdentity(uuid::Uuid);

impl ReplicatorCreationIdentity {
    pub(crate) fn new(_token: RuntimeHostToken) -> Self {
        Self(uuid::Uuid::new_v4())
    }

    pub(crate) fn to_public_id(self) -> ReplicatorCreationId {
        ReplicatorCreationId::new(self.0.to_string())
    }
}
