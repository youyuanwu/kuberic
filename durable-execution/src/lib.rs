//! Experimental durable-execution kernel primitives.
//!
//! This crate is isolated from Kuberic production components. Its current
//! ordinary-async authoring API is paired with a runtime-neutral asynchronous
//! provider and host contract. Dispatch permission is available only after
//! separate accepted schedule and exposure checkpoint transitions. The crate
//! owns no executor and does not depend on Tokio.
//!
//! The crate README documents the selected
//! [ordinary-async authoring surface](../README.md#selected-authoring-surface),
//! [replay and checkpoint semantics](../README.md#replay-and-checkpoint-semantics),
//! [turn and dispatch-permission boundary](../README.md#turns-and-dispatch-permission),
//! [quarantine recovery](../README.md#quarantine-and-observation-recovery), and
//! [bounded limitations](../README.md#limitations-and-exclusions).

mod assessment;
mod checkpoint;
mod host;
mod identity;
mod in_memory;
mod replay;
mod store;
mod workflow;

pub use assessment::{FeasibilityClassification, FeasibilityInputs, classify_feasibility};
pub use checkpoint::{
    ActivityRecord, ActivityState, CHECKPOINT_FORMAT_VERSION, CheckpointEnvelope, CheckpointError,
    CheckpointLimits, CheckpointPayload,
};
pub use host::{
    ActivityObservation, DispatchPermit, DurableHost, HOST_OUTCOME_VARIANTS, HostOutcome,
    ObservationRejection, PersistenceBoundary, ReloadReason, StoreOperation,
};
pub use identity::{
    ActivityName, ActivitySequence, ActivitySpec, AttemptId, ExactBytes, ExecutionId, HostEpoch,
    IdentityError, LogicalActivityId,
};
pub use in_memory::{InMemoryCheckpointStore, InMemoryFault};
pub use replay::{Evaluation, Nondeterminism, evaluate};
pub use store::{
    CasOutcome, CheckpointStore, StorageRevision, StoreError, StoreErrorKind, StoredCheckpoint,
};
pub use workflow::{Workflow, WorkflowContext};
