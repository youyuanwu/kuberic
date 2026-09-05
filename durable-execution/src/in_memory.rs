use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use crate::{
    CheckpointEnvelope, CheckpointStore, CompareAndSwap, ExecutionId, StorageRevision,
    StoredCheckpoint,
};

/// One-shot behavior for the next compare-and-swap whose expected revision matches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InMemoryFault {
    RejectBeforeApply,
    LoseResponseAfterApply,
}

/// Cloneable, process-local checkpoint store used by the conformance harness.
#[derive(Clone, Default)]
pub struct InMemoryCheckpointStore {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    checkpoints: BTreeMap<ExecutionId, StoredCheckpoint>,
    next_fault: Option<InMemoryFault>,
}

impl InMemoryCheckpointStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Arm a fault for one matching compare-and-swap. A newly armed fault replaces
    /// an older unconsumed fault.
    pub fn fail_next_compare_and_swap(&self, fault: InMemoryFault) {
        self.state
            .lock()
            .expect("in-memory checkpoint store mutex poisoned")
            .next_fault = Some(fault);
    }
}

impl CheckpointStore for InMemoryCheckpointStore {
    fn load(&self, execution_id: ExecutionId) -> Option<StoredCheckpoint> {
        self.state
            .lock()
            .expect("in-memory checkpoint store mutex poisoned")
            .checkpoints
            .get(&execution_id)
            .cloned()
    }

    fn compare_and_swap(
        &self,
        execution_id: ExecutionId,
        expected: Option<StorageRevision>,
        checkpoint: CheckpointEnvelope,
    ) -> CompareAndSwap {
        let mut state = self
            .state
            .lock()
            .expect("in-memory checkpoint store mutex poisoned");
        let actual = state
            .checkpoints
            .get(&execution_id)
            .map(StoredCheckpoint::revision);
        if actual != expected {
            return CompareAndSwap::Conflict;
        }

        let fault = state.next_fault.take();
        match fault {
            Some(InMemoryFault::RejectBeforeApply) => CompareAndSwap::RejectedBeforeApply,
            fault @ (None | Some(InMemoryFault::LoseResponseAfterApply)) => {
                let revision = actual
                    .map(StorageRevision::next)
                    .unwrap_or_else(StorageRevision::initial);
                state
                    .checkpoints
                    .insert(execution_id, StoredCheckpoint::new(revision, checkpoint));
                if matches!(fault, Some(InMemoryFault::LoseResponseAfterApply)) {
                    CompareAndSwap::ResponseLostAfterApply
                } else {
                    CompareAndSwap::Accepted(revision)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CheckpointPayload, ExactBytes};

    fn checkpoint(execution_id: ExecutionId, marker: u8) -> CheckpointEnvelope {
        CheckpointEnvelope::encode(&CheckpointPayload::new(
            execution_id,
            ExactBytes::new([marker]),
            Vec::new(),
        ))
        .unwrap()
    }

    #[test]
    fn compare_and_swap_is_atomic_and_revisions_advance_only_on_apply() {
        let store = InMemoryCheckpointStore::new();
        let execution_id = ExecutionId::from_bytes([1; 16]);

        let CompareAndSwap::Accepted(first) =
            store.compare_and_swap(execution_id, None, checkpoint(execution_id, 1))
        else {
            panic!("initial creation was not accepted");
        };
        assert_eq!(
            store.compare_and_swap(execution_id, None, checkpoint(execution_id, 2)),
            CompareAndSwap::Conflict
        );

        store.fail_next_compare_and_swap(InMemoryFault::RejectBeforeApply);
        assert_eq!(
            store.compare_and_swap(execution_id, Some(first), checkpoint(execution_id, 3)),
            CompareAndSwap::RejectedBeforeApply
        );
        assert_eq!(store.load(execution_id).unwrap().revision(), first);

        store.fail_next_compare_and_swap(InMemoryFault::LoseResponseAfterApply);
        assert_eq!(
            store.compare_and_swap(execution_id, Some(first), checkpoint(execution_id, 4)),
            CompareAndSwap::ResponseLostAfterApply
        );
        let second = store.load(execution_id).unwrap().revision();
        assert_ne!(first, second);
    }
}
