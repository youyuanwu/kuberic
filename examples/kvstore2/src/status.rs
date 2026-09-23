use std::sync::Arc;

use kuberic_agent::hosting::PodRuntime;
use kuberic_agent::store::AgentStore;
use serde::Serialize;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplicaDiagnostics {
    pub replica_id: i64,
    pub instance_id: String,
    pub agent_generation: String,
    pub process_session: String,
    pub role: String,
    pub epoch: String,
    pub previous_configuration: Option<String>,
    pub current_configuration: Option<String>,
    pub current_progress: i64,
    pub committed_lsn: i64,
    pub write_status: String,
}

pub async fn diagnostics<S: AgentStore>(
    runtime: &Arc<PodRuntime>,
    store: &Arc<S>,
    process_session: &str,
) -> kuberic_agent::Result<ReplicaDiagnostics> {
    let state = store.load_state().await?;
    let snapshot = runtime.snapshot().await;
    Ok(ReplicaDiagnostics {
        replica_id: state.identity.local_identity.replica_id.value(),
        instance_id: state.identity.local_identity.instance_id.to_string(),
        agent_generation: state.identity.local_identity.agent_generation.to_string(),
        process_session: process_session.to_string(),
        role: format!("{:?}", snapshot.role),
        epoch: format!(
            "{}.{}",
            state.highest_epoch.data_loss_number, state.highest_epoch.configuration_number
        ),
        previous_configuration: state
            .previous_configuration
            .map(|configuration| configuration.configuration_id.to_string()),
        current_configuration: state
            .current_configuration
            .map(|configuration| configuration.configuration_id.to_string()),
        current_progress: snapshot.current_progress,
        committed_lsn: snapshot.committed_lsn,
        write_status: format!("{:?}", snapshot.write_status),
    })
}
