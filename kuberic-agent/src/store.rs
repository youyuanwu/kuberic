//! Narrow agent store operations.

use async_trait::async_trait;
use kuberic_runtime_internal::effects::{RuntimeEffect, RuntimeEffectResult};

use crate::Result;
use crate::state::{AgentState, RetainedResult, StorageIdentity};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginEffect {
    Execute(RuntimeEffect),
    Pending(RuntimeEffect),
    Completed(Box<RuntimeEffectResult>),
}

#[async_trait]
pub trait AgentStore: Send + Sync {
    async fn identity(&self) -> Result<StorageIdentity>;

    async fn load_state(&self) -> Result<AgentState>;

    async fn begin_effect(&self, effect: &RuntimeEffect) -> Result<BeginEffect>;

    async fn mark_effect_applied(&self, effect: &RuntimeEffect) -> Result<()>;

    async fn complete_effect(&self, result: &RuntimeEffectResult) -> Result<()>;

    async fn retained_result(&self) -> Result<Option<RetainedResult>>;

    async fn set_reconfiguration(&self, data: Option<String>) -> Result<()>;

    async fn clear_reconfiguration(&self) -> Result<()>;

    async fn migrate_schema(&self, expected_version: u32, target_version: u32) -> Result<()>;
}
