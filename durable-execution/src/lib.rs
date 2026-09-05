//! Experimental deterministic replay primitives.
//!
//! This crate is isolated from Kuberic production components. Its current
//! ordinary-async authoring API is provisional and intentionally exposes no
//! host, storage, compare-and-swap, or dispatch-permission surface.

mod checkpoint;
mod identity;
mod replay;
mod workflow;

pub use checkpoint::{
    ActivityRecord, ActivityState, CHECKPOINT_FORMAT_VERSION, CheckpointEnvelope, CheckpointError,
    CheckpointPayload,
};
pub use identity::{
    ActivityName, ActivitySequence, AttemptId, ExactBytes, ExecutionId, HostEpoch, IdentityError,
    LogicalActivityId,
};
pub use replay::{Evaluation, Nondeterminism, evaluate};
pub use workflow::{Workflow, WorkflowContext};
