//! Experimental deterministic replay primitives.
//!
//! This crate is isolated from Kuberic production components. Its current
//! ordinary-async authoring API is paired with a synchronous in-memory durable
//! host. Dispatch permission is available only after separate accepted
//! schedule and exposure checkpoint transitions.

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
    CheckpointPayload,
};
pub use host::{
    ActivityObservation, DispatchPermit, DurableHost, HOST_OUTCOME_VARIANTS, HostOutcome,
    ObservationRejection, PersistenceBoundary, ReloadReason,
};
pub use identity::{
    ActivityName, ActivitySequence, AttemptId, ExactBytes, ExecutionId, HostEpoch, IdentityError,
    LogicalActivityId,
};
pub use in_memory::{InMemoryCheckpointStore, InMemoryFault};
pub use replay::{Evaluation, Nondeterminism, evaluate};
pub use store::{CheckpointStore, CompareAndSwap, StorageRevision, StoredCheckpoint};
pub use workflow::{Workflow, WorkflowContext};
