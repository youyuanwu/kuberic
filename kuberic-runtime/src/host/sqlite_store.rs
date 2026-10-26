//! SQLite-backed agent persistence.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::protocol::public_operations::PublicV1Identity;
use crate::protocol::public_operations::PublicV1LifecycleBinding;
use crate::protocol::public_operations::{
    DataLossRenewalDecision, PossibleDataLossIntent, PublicBuildInput, PublicOperationIntent,
};
use crate::protocol::types::{FaultType, LoadMetric, OperationId, ReplicaRole};
use async_trait::async_trait;
use rusqlite::{Connection, OpenFlags, Transaction};

use crate::host::Result;
use crate::host::public_build_store::{PublicV1BuildRecord, PublicV1BuildRetirement};
use crate::host::state::PublicOperationContainment;
use crate::host::state::{
    AgentState, PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION, PublicInstructionOutcome,
    PublicOperationDisposition, PublicOperationRecord, PublicOperationStage,
    PublicV1OperationState, StorageIdentity,
};
use crate::host::store::{AgentStore, BeginPublicOperation, RenewPublicDataLossAttempt};

pub(crate) const PUBLIC_V1_AGENT_TABLES: &[&str] = &["agent_state"];

const DATABASE_FILE: &str = "agent.sqlite3";

fn validate_public_v1_state(state: &AgentState) -> Result<()> {
    let preview = state.public_v1_operation_state.as_ref().ok_or_else(|| {
        crate::host::HostError::Corrupt("replacement store lacks Public V1 state".into())
    })?;
    if !preview.identity.is_public_v1() {
        return Err(crate::host::HostError::Corrupt(
            "replacement store contains another protocol identity".into(),
        ));
    }
    for (operation_id, operation) in &preview.operations {
        operation.intent.validate().map_err(|message| {
            crate::host::HostError::Corrupt(format!(
                "invalid replacement operation {operation_id}: {message}"
            ))
        })?;
        if operation_id != &operation.intent.operation_id {
            return Err(crate::host::HostError::Corrupt(
                "replacement operation key differs from its intent".into(),
            ));
        }
        for outcome in &operation.lifecycle.outcomes {
            if let PublicInstructionOutcome::RecoveryExitRequired(notice) = outcome {
                if &notice.fault_operation_id != operation_id {
                    return Err(crate::host::HostError::Corrupt(
                        "recovery cause is attached to another operation".into(),
                    ));
                }
                if operation.containment != PublicOperationContainment::Pending {
                    return Err(crate::host::HostError::Corrupt(
                        "recovery exit notice cannot certify successful Cleanup".into(),
                    ));
                }
                super::ordinary_goal::retained::validate_persisted_notice(state, notice)
                    .map_err(|error| crate::host::HostError::Corrupt(error.to_string()))?;
            }
            if let PublicInstructionOutcome::RetainedNativeInitialized(scope) = outcome
                && operation.intent.goal().is_none_or(|goal| {
                    !matches!(goal.desired, crate::protocol::public_operations::ReplicaGoalKind::RestoreRetainedMember { .. })
                        || scope.as_ref() != &goal.scope
                })
            {
                return Err(crate::host::HostError::Corrupt("native retained receipt is outside its exact restoration".into()));
            }
        }
        if let Some(goal) = operation.intent.goal() {
            let ordinary = operation.ordinary.as_ref().ok_or_else(|| {
                crate::host::HostError::Corrupt("semantic goal lacks frozen local actions".into())
            })?;
            let mut expected =
                crate::host::public_lifecycle::operation_instructions(&operation.intent);
            if ordinary.already_open {
                if !matches!(
                    goal.desired,
                    crate::protocol::public_operations::ReplicaGoalKind::Prepare { .. }
                ) || !super::ordinary_goal::opened_creation(state, goal)
                {
                    return Err(crate::host::HostError::Corrupt(
                        "frozen Open prefix lacks its successful creation receipt".into(),
                    ));
                }
                expected.retain(|instruction| {
                    !matches!(
                        instruction,
                        crate::host::state::PublicInstruction::ApplicationOpen
                            | crate::host::state::PublicInstruction::ReplicatorOpen
                    )
                });
            }
            let tombstone = matches!(
                goal.desired,
                crate::protocol::public_operations::ReplicaGoalKind::Remove { .. }
            ) && ordinary.actions.is_empty();
            if ordinary.action_format != crate::protocol::public_operations::ORDINARY_ACTION_FORMAT
                || ordinary.open_existing.is_some()
                    != matches!(
                        goal.desired,
                        crate::protocol::public_operations::ReplicaGoalKind::Discover
                            | crate::protocol::public_operations::ReplicaGoalKind::Prepare { .. }
                            | crate::protocol::public_operations::ReplicaGoalKind::RestoreRetainedMember { .. }
                    )
                || (!tombstone && ordinary.actions != expected)
                || operation.lifecycle.outcomes.len() > ordinary.actions.len()
                || operation.lifecycle.in_flight.is_some_and(|instruction| {
                    ordinary.actions.get(operation.lifecycle.outcomes.len()) != Some(&instruction)
                })
                || operation.accepted_history.as_ref() != Some(&goal.scope.history)
                || operation.accepted_storage.as_ref() != Some(&goal.scope.storage)
            {
                return Err(crate::host::HostError::Corrupt(
                    "semantic goal frozen actions or scope changed".into(),
                ));
            }
            super::ordinary_goal::validate_frozen_admissions(state, operation)
                .map_err(|error| crate::host::HostError::Corrupt(error.to_string()))?;
            if matches!(
                goal.desired,
                crate::protocol::public_operations::ReplicaGoalKind::RestoreRetainedMember { .. }
            ) {
                super::ordinary_goal::retained::validate_persisted_receipt(state, operation)
                    .map_err(|error| crate::host::HostError::Corrupt(error.to_string()))?;
            } else if ordinary.retained.is_some() {
                return Err(crate::host::HostError::Corrupt(
                    "retained custody attached to another recipe".into(),
                ));
            }
        } else if operation.ordinary.is_some() {
            return Err(crate::host::HostError::Corrupt(
                "ordinary actions lack their canonical goal".into(),
            ));
        }
        operation
            .accepted_history
            .as_ref()
            .ok_or_else(|| {
                crate::host::HostError::Corrupt(
                    "replacement operation lacks accepted history".into(),
                )
            })?
            .validate()
            .map_err(|message| crate::host::HostError::Corrupt(message.into()))?;
        operation
            .accepted_storage
            .as_ref()
            .ok_or_else(|| {
                crate::host::HostError::Corrupt(
                    "replacement operation lacks accepted storage incarnation".into(),
                )
            })?
            .validate()
            .map_err(|message| crate::host::HostError::Corrupt(message.into()))?;
    }
    for (operation_id, build_record) in &preview.build_records {
        build_record
            .validate()
            .map_err(|message| crate::host::HostError::Corrupt(message.into()))?;
        if operation_id != &build_record.operation_id {
            return Err(crate::host::HostError::Corrupt(
                "public-v1 build record key differs from its operation".into(),
            ));
        }
        let operation = preview.operations.get(operation_id).ok_or_else(|| {
            crate::host::HostError::Corrupt(
                "public-v1 build record lacks its durable operation".into(),
            )
        })?;
        let Some(crate::protocol::public_operations::PublicOperationProgram::Build(build)) =
            operation.intent.program.as_ref()
        else {
            return Err(crate::host::HostError::Corrupt(
                "public-v1 build record operation has another call kind".into(),
            ));
        };
        if operation.intent.revision != build_record.admission_revision
            || operation.accepted_history.as_ref() != Some(&build_record.history)
            || operation.accepted_storage.as_ref() != Some(&build_record.source_storage)
            || build.attempt != build_record.build_id
            || build.epoch != build_record.epoch
            || build.history.as_ref() != Some(&build_record.history)
            || build.replica != build_record.target
            || build.target_storage != build_record.target_storage
            || build.process_session_id != build_record.target_session
            || build.replication_address != build_record.target_address
            || operation.intent.process_session_id != build_record.source_session
        {
            return Err(crate::host::HostError::Corrupt(
                "public-v1 build record differs from its accepted intent".into(),
            ));
        }
        let expected_result = public_v1_build_result(operation);
        if build_record.result != expected_result
            || (operation.stage == PublicOperationStage::Completed && build_record.result.is_none())
        {
            return Err(crate::host::HostError::Corrupt(
                "public-v1 build record terminal result differs from its operation".into(),
            ));
        }
    }
    for storage in &preview.retired_storage_incarnations {
        storage
            .validate()
            .map_err(|message| crate::host::HostError::Corrupt(message.into()))?;
    }
    Ok(())
}

fn public_v1_build_result(
    operation: &PublicOperationRecord,
) -> Option<crate::protocol::public_operations::PublicV1OperationResult> {
    if let Some(PublicInstructionOutcome::RebuildRequired(rebuild)) = operation
        .lifecycle
        .outcomes
        .iter()
        .find(|outcome| matches!(outcome, PublicInstructionOutcome::RebuildRequired(_)))
    {
        return Some(
            crate::protocol::public_operations::PublicV1OperationResult::RebuildRequired(
                rebuild.clone(),
            ),
        );
    }
    operation
        .disposition
        .as_ref()
        .map(|disposition| match disposition {
            PublicOperationDisposition::Succeeded => {
                crate::protocol::public_operations::PublicV1OperationResult::Succeeded
            }
            PublicOperationDisposition::Failed(message) => {
                crate::protocol::public_operations::PublicV1OperationResult::Failed(message.clone())
            }
            PublicOperationDisposition::Ambiguous(message) => {
                crate::protocol::public_operations::PublicV1OperationResult::Ambiguous(
                    message.clone(),
                )
            }
            PublicOperationDisposition::Cancelled => {
                crate::protocol::public_operations::PublicV1OperationResult::Cancelled
            }
            PublicOperationDisposition::Attached(owner) => {
                crate::protocol::public_operations::PublicV1OperationResult::Ambiguous(format!(
                    "attached to {owner}"
                ))
            }
        })
}

pub(crate) struct SqliteStore {
    #[cfg(any(all(test, kuberic_workspace_tests), feature = "testing"))]
    path: PathBuf,
    connection: Mutex<Connection>,
    process_storage: Mutex<Option<Vec<File>>>,
    process_owner: Mutex<Option<crate::protocol::types::ProcessSessionId>>,
}

impl Drop for SqliteStore {
    fn drop(&mut self) {
        // Lost/failed cleanup is not a writer-release receipt. Keep the OS
        // ownership until process death even if the last diagnostic handle drops.
        if let Some(files) = self.process_storage.get_mut().unwrap().take() {
            std::mem::forget(files);
        }
    }
}

impl SqliteStore {
    pub(crate) fn bind_process_owner(
        &self,
        process: &crate::protocol::types::ProcessSessionId,
    ) -> Result<()> {
        let custody = self.process_storage.lock().unwrap();
        let mut owner = self.process_owner.lock().unwrap();
        if process.is_empty()
            || custody.as_ref().is_none_or(Vec::is_empty)
            || owner.as_ref().is_some_and(|old| old != process)
        {
            return Err(crate::host::HostError::CommandRejected(
                "process binding requires actual exclusive storage lifetime ownership".into(),
            ));
        }
        *owner = Some(process.clone());
        Ok(())
    }

    fn owns_process(&self, process: &crate::protocol::types::ProcessSessionId) -> bool {
        self.process_storage
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|files| !files.is_empty())
            && self.process_owner.lock().unwrap().as_ref() == Some(process)
    }

    pub(crate) fn read_retained_recovery_state(path: &Path) -> Result<AgentState> {
        classify_replacement_format(path)?;
        let connection = open_read_only_connection_with_immutable(path, false)?;
        let state = load_state_from_connection(&connection)?;
        if state.identity.schema_version != PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION {
            return Err(crate::host::HostError::SchemaMismatch {
                expected: PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION,
                observed: state.identity.schema_version,
            });
        }
        validate_public_v1_state(&state)?;
        Ok(state)
    }

    pub(crate) fn release_process_storage(&self) {
        self.process_storage.lock().unwrap().take();
    }

    fn lock_process_storage(
        path: &Path,
        application_paths: Option<&BTreeMap<String, PathBuf>>,
    ) -> Result<Vec<File>> {
        let mut locks = vec![path.with_extension("owner")];
        for application in application_paths
            .into_iter()
            .flat_map(|paths| paths.values())
        {
            let name = application.file_name().ok_or_else(|| {
                crate::host::HostError::InitializationNotAuthorized(
                    "application storage must name a storage resource".into(),
                )
            })?;
            locks.push(
                application.with_file_name(format!(".{}.kuberic-owner", name.to_string_lossy())),
            );
        }
        locks.sort();
        locks.dedup();
        let mut files = Vec::new();
        for lock in locks {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&lock)?;
            file.try_lock().map_err(|error| {
                crate::host::HostError::CommandRejected(format!(
                    "storage lifetime is already owned at {}: {error}",
                    lock.display()
                ))
            })?;
            files.push(file);
        }
        Ok(files)
    }

    pub(crate) fn metadata_database_path(pvc_root: &Path) -> PathBuf {
        pvc_root.join(".kuberic").join(DATABASE_FILE)
    }

    pub(crate) fn create_replacement_authorized(
        path: impl AsRef<Path>,
        mut state: AgentState,
        binding: PublicV1LifecycleBinding,
    ) -> Result<Self> {
        binding.validate().map_err(|message| {
            crate::host::HostError::InitializationNotAuthorized(message.into())
        })?;
        if !binding.identity.is_public_v1() {
            return Err(crate::host::HostError::InitializationNotAuthorized(
                "replacement store requires Public V1".into(),
            ));
        }
        if state.identity.resource_uid != binding.resource_uid {
            return Err(crate::host::HostError::IdentityMismatch(
                "replacement binding resource UID differs from store identity".into(),
            ));
        }
        if state.public_v1_operation_state.is_some() {
            return Err(crate::host::HostError::InitializationNotAuthorized(
                "Public V1 replacement state is already initialized".into(),
            ));
        }
        state.identity.schema_version = PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION;
        state.public_v1_operation_state = Some(PublicV1OperationState::new_bound(binding));
        Self::create_store(path, state)
    }

    fn create_store(path: impl AsRef<Path>, state: AgentState) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if state.public_v1_operation_state.is_none() {
            return Err(crate::host::HostError::InitializationNotAuthorized(
                "replacement store requires Public V1 state".into(),
            ));
        }
        if state.identity.schema_version != PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION {
            return Err(crate::host::HostError::SchemaMismatch {
                expected: PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION,
                observed: state.identity.schema_version,
            });
        }
        let parent = path.parent().ok_or_else(|| {
            crate::host::HostError::Corrupt("agent database path has no metadata directory".into())
        })?;
        std::fs::create_dir_all(parent)?;
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let result = (|| {
            let mut connection = open_writable_connection(&path)?;
            configure_durability(&connection)?;
            create_schema(&mut connection, &state)?;
            Ok(Self {
                #[cfg(any(all(test, kuberic_workspace_tests), feature = "testing"))]
                path: path.clone(),
                connection: Mutex::new(connection),
                process_storage: Mutex::new(None),
                process_owner: Mutex::new(None),
            })
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&path);
        }
        result
    }

    pub(crate) fn open_replacement_existing(
        path: impl AsRef<Path>,
        expected_identity: Option<&StorageIdentity>,
        expected_protocol: &PublicV1Identity,
    ) -> Result<Self> {
        if !expected_protocol.is_valid() {
            return Err(crate::host::HostError::InitializationNotAuthorized(
                "invalid Public V1 protocol identity".into(),
            ));
        }
        if !expected_protocol.is_public_v1() {
            return Err(crate::host::HostError::InitializationNotAuthorized(
                "replacement store requires Public V1".into(),
            ));
        }
        let path = path.as_ref().to_path_buf();
        classify_replacement_store(&path, expected_protocol, None, |actual| {
            validate_expected_identity(actual, expected_identity)
        })?;
        let connection = open_writable_connection(&path).map_err(|error| {
            crate::host::HostError::Corrupt(format!("cannot open SQLite metadata: {error}"))
        })?;
        configure_durability(&connection)
            .map_err(|error| crate::host::HostError::Corrupt(error.to_string()))?;
        Ok(Self {
            #[cfg(any(all(test, kuberic_workspace_tests), feature = "testing"))]
            path,
            connection: Mutex::new(connection),
            process_storage: Mutex::new(None),
            process_owner: Mutex::new(None),
        })
    }

    pub(crate) fn open_replacement_bound_existing(
        path: impl AsRef<Path>,
        expected_identity: Option<&StorageIdentity>,
        expected_binding: &PublicV1LifecycleBinding,
    ) -> Result<Self> {
        expected_binding.validate().map_err(|message| {
            crate::host::HostError::InitializationNotAuthorized(message.into())
        })?;
        let path = path.as_ref().to_path_buf();
        classify_replacement_store(
            &path,
            &expected_binding.identity,
            Some(expected_binding),
            |actual| validate_expected_identity(actual, expected_identity),
        )?;
        let connection = open_writable_connection(&path).map_err(|error| {
            crate::host::HostError::Corrupt(format!("cannot open SQLite metadata: {error}"))
        })?;
        configure_durability(&connection)
            .map_err(|error| crate::host::HostError::Corrupt(error.to_string()))?;
        Ok(Self {
            #[cfg(any(all(test, kuberic_workspace_tests), feature = "testing"))]
            path,
            connection: Mutex::new(connection),
            process_storage: Mutex::new(None),
            process_owner: Mutex::new(None),
        })
    }

    pub(crate) fn open_replacement_for_process(
        path: impl AsRef<Path>,
        observed: &crate::host::provisioning::ObservedStorageIdentity,
        replica_id: crate::protocol::types::ReplicaId,
        application_storage_paths: Option<&BTreeMap<String, PathBuf>>,
        expected_binding: &PublicV1LifecycleBinding,
    ) -> Result<Self> {
        expected_binding.validate().map_err(|message| {
            crate::host::HostError::InitializationNotAuthorized(message.into())
        })?;
        let path = path.as_ref().to_path_buf();
        classify_replacement_format(&path)?;
        // Lock the existing inode without creating anything. Matching-format
        // journal recovery must not race another process, while full retained
        // format/path validation still precedes all owner-file creation.
        let metadata = File::open(&path)?;
        metadata.try_lock().map_err(|error| {
            crate::host::HostError::CommandRejected(format!(
                "storage lifetime is already owned at {}: {error}",
                path.display()
            ))
        })?;
        let state = classify_replacement_store(
            &path,
            &expected_binding.identity,
            Some(expected_binding),
            |actual| {
                crate::host::provisioning::validate_established_identity(
                    actual, observed, replica_id,
                )
            },
        )?;
        if state
            .application_storage
            .as_ref()
            .map(|binding| &binding.paths)
            != application_storage_paths
        {
            return Err(crate::host::HostError::InitializationNotAuthorized(
                "application storage paths differ from the authorized agent binding".into(),
            ));
        }
        let mut process_storage = Self::lock_process_storage(&path, application_storage_paths)?;
        process_storage.push(metadata);
        let connection = open_writable_connection(&path).map_err(|error| {
            crate::host::HostError::Corrupt(format!("cannot open SQLite metadata: {error}"))
        })?;
        configure_durability(&connection)
            .map_err(|error| crate::host::HostError::Corrupt(error.to_string()))?;
        Ok(Self {
            #[cfg(any(all(test, kuberic_workspace_tests), feature = "testing"))]
            path,
            connection: Mutex::new(connection),
            process_storage: Mutex::new(Some(process_storage)),
            process_owner: Mutex::new(None),
        })
    }

    #[cfg(any(all(test, kuberic_workspace_tests), feature = "testing"))]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    fn with_transaction<T>(&self, action: impl FnOnce(&Transaction<'_>) -> Result<T>) -> Result<T> {
        let mut connection = self.connection.lock().map_err(|_| {
            crate::host::HostError::Corrupt("agent database connection mutex was poisoned".into())
        })?;
        let transaction = connection.transaction()?;
        let result = action(&transaction)?;
        transaction.commit()?;
        Ok(result)
    }
}

fn build_was_promoted(preview: &PublicV1OperationState, build: &PublicBuildInput) -> bool {
    preview.build_was_promoted(build)
}

#[async_trait]
impl AgentStore for SqliteStore {
    async fn acknowledge_ordinary_goal(
        &self,
        acknowledgement: &crate::protocol::public_operations::ReplicaGoalAcknowledgement,
    ) -> Result<()> {
        let goal = &acknowledgement.goal;
        goal.validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        self.with_transaction(|transaction| {
            let mut state = load_state_from_connection(transaction)?;
            super::ordinary_goal::validate_acknowledgement(&state, acknowledgement)?;
            let preview = state.public_v1_operation_state.as_mut().ok_or_else(|| {
                crate::host::HostError::CommandRejected("ordinary goal store unavailable".into())
            })?;
            let record = preview
                .operations
                .get_mut(&goal.operation_id)
                .ok_or_else(|| {
                    crate::host::HostError::CommandRejected("unknown ordinary goal".into())
                })?;
            let ordinary = record.ordinary.as_mut().ok_or_else(|| {
                crate::host::HostError::Corrupt("ordinary goal lacks frozen actions".into())
            })?;
            ordinary.accepted = true;
            ordinary.host_settled = record.stage == PublicOperationStage::Completed;
            if let Some(remote) = goal.remote() {
                let key = super::ordinary_goal::remote_key(remote);
                let watermark = (goal.scope.epoch, remote.generation);
                // Reclamation never precedes a newer accepted negative boundary.
                if ordinary.logically_retired {
                    preview
                        .remote_watermarks
                        .entry(key.clone())
                        .and_modify(|accepted| *accepted = (*accepted).max(watermark))
                        .or_insert(watermark);
                    let admission_dependencies = preview
                        .operations
                        .values()
                        .filter_map(|record| record.ordinary.as_ref())
                        .flat_map(|ordinary| ordinary.ready_admissions.iter().cloned())
                        .collect::<Vec<_>>();
                    preview.operations.retain(|_, record| {
                        let below = record
                            .intent
                            .goal()
                            .and_then(|goal| {
                                goal.remote().map(|remote| {
                                    super::ordinary_goal::remote_key(remote) == key
                                        && (goal.scope.epoch, remote.generation) < watermark
                                })
                            })
                            .unwrap_or(false);
                        !(below
                            && !record
                                .intent
                                .goal()
                                .and_then(|goal| goal.remote())
                                .is_some_and(|remote| admission_dependencies.contains(remote))
                            && record.stage == PublicOperationStage::Completed
                            && record.lifecycle.in_flight.is_none()
                            && record.ordinary.as_ref().is_some_and(|ordinary| {
                                ordinary.accepted
                                    && ordinary.logically_retired
                                    && ordinary.host_settled
                            }))
                    });
                }
            }
            write_agent_state(transaction, &state)
        })
    }
    async fn identity(&self) -> Result<StorageIdentity> {
        Ok(self.load_state().await?.identity)
    }

    async fn load_state(&self) -> Result<AgentState> {
        let connection = self.connection.lock().map_err(|_| {
            crate::host::HostError::Corrupt("agent database connection mutex was poisoned".into())
        })?;
        load_state_from_connection(&connection)
    }

    async fn complete_application_initialization(&self) -> Result<()> {
        self.with_transaction(|transaction| {
            let mut state = load_state_from_connection(transaction)?;
            if let Some(binding) = &mut state.application_storage
                && binding.initializing
            {
                binding.initializing = false;
                write_agent_state(transaction, &state)?;
            }
            Ok(())
        })
    }

    async fn record_partition_reports(
        &self,
        load_metrics: Vec<LoadMetric>,
        reported_fault: Option<FaultType>,
    ) -> Result<()> {
        self.with_transaction(|transaction| {
            let mut names = std::collections::BTreeSet::new();
            if load_metrics.iter().any(|metric| {
                metric.name.is_empty() || metric.value < 0 || !names.insert(metric.name.clone())
            }) {
                return Err(crate::host::HostError::CommandRejected(
                    "load metrics require unique nonempty names and nonnegative values".into(),
                ));
            }
            let mut state = load_state_from_connection(transaction)?;
            state.load_metrics = load_metrics;
            state.reported_fault = reported_fault;
            write_agent_state(transaction, &state)
        })
    }

    async fn initialize_public_v1_history(
        &self,
        initialization_id: &crate::protocol::types::HistoryInitializationId,
        history: &crate::protocol::types::HistoryContext,
        storage: &crate::protocol::types::ReplicaStorageIncarnationContext,
    ) -> Result<()> {
        history
            .validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        storage
            .validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        self.with_transaction(|transaction| {
            let mut state = load_state_from_connection(transaction)?;
            let preview = state.public_v1_operation_state.as_mut().ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "Public V1 replacement store required".into(),
                )
            })?;
            if !preview.identity.is_public_v1()
                || initialization_id.is_empty()
                || history.resource_uid != state.identity.resource_uid
                || history.protocol_generation.value() != preview.identity.generation
                || storage.replica != state.identity.local_identity
            {
                return Err(crate::host::HostError::IdentityMismatch(
                    "public-v1 history initialization differs from store identity".into(),
                ));
            }
            if let crate::protocol::types::ReplicaStorageIdentity::Persistent { pod_uid, pvc_uid } =
                &storage.storage
                && (pod_uid != &state.identity.pod_uid || pvc_uid != &state.identity.pvc_uid)
            {
                return Err(crate::host::HostError::IdentityMismatch(
                    "public-v1 storage incarnation differs from local storage".into(),
                ));
            }
            let expected = crate::protocol::types::derive_bootstrap_history(
                &state.identity.resource_uid,
                crate::protocol::types::PublicProtocolGeneration::new(preview.identity.generation),
                initialization_id,
            );
            if history != &expected {
                return Err(crate::host::HostError::CommandRejected(
                    "public-v1 bootstrap history differs from accepted initialization".into(),
                ));
            }
            let existing = (
                preview.history_initialization_id.as_ref(),
                preview.history.as_ref(),
                preview.storage_incarnation.as_ref(),
            );
            if existing.0.is_some() || existing.1.is_some() || existing.2.is_some() {
                if existing != (Some(initialization_id), Some(history), Some(storage)) {
                    return Err(crate::host::HostError::DurableEffectConflict(
                        "public-v1 history initialization changed".into(),
                    ));
                }
                return Ok(());
            }
            preview.history_initialization_id = Some(initialization_id.clone());
            preview.history = Some(history.clone());
            preview.storage_incarnation = Some(storage.clone());
            write_agent_state(transaction, &state)
        })
    }

    async fn persist_public_v1_session_renewal(
        &self,
        expected: Option<&crate::protocol::public_operations::PublicV1SessionRenewalStatus>,
        next: &crate::protocol::public_operations::PublicV1SessionRenewalStatus,
    ) -> Result<()> {
        next.validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.into()))?;
        self.with_transaction(|transaction| {
            let mut state = load_state_from_connection(transaction)?;
            let preview = state.public_v1_operation_state.as_mut().ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "Public V1 replacement store required".into(),
                )
            })?;
            if !preview.identity.is_public_v1()
                || preview.history.as_ref() != Some(&next.required.history)
                || next.required.resource_uid != state.identity.resource_uid
                || !std::iter::once(&next.required.predecessor)
                    .chain(next.required.replacement.iter())
                    .chain(next.required.surviving_sessions.iter())
                    .any(|endpoint| endpoint.identity == state.identity.local_identity)
            {
                return Err(crate::host::HostError::IdentityMismatch(
                    "public-v1 session renewal differs from local durable authority".into(),
                ));
            }
            if next.required.predecessor.identity == state.identity.local_identity
                && preview.storage_incarnation.as_ref()
                    != Some(&next.required.predecessor.storage_incarnation)
            {
                return Err(crate::host::HostError::IdentityMismatch(
                    "public-v1 session renewal changed predecessor storage".into(),
                ));
            }
            if let Some(data_loss) = &next.required.data_loss
                && (preview.data_loss_authorization.as_ref() != Some(&data_loss.authorization)
                    || preview.data_loss_recovery.as_ref().is_none_or(|recovery| {
                        recovery.logical_authorization() != data_loss.authorization
                            || recovery.attempt != data_loss.predecessor_attempt
                    }))
            {
                return Err(crate::host::HostError::IdentityMismatch(
                    "public-v1 session renewal changed durable data-loss authority".into(),
                ));
            }
            match (&preview.session_renewal, expected) {
                (Some(current), Some(expected)) if current == expected => {
                    if current == next {
                        return Ok(());
                    }
                    if !next.is_successor_of(current)
                        && !crate::host::session_renewal_supersedes_local_journal(next, current)
                    {
                        return Err(crate::host::HostError::StaleEffectCompletion(
                            "public-v1 session renewal transition is out of order".into(),
                        ));
                    }
                }
                (None, None) => {}
                _ => {
                    return Err(crate::host::HostError::DurableEffectConflict(
                        "public-v1 session renewal changed accepted state".into(),
                    ));
                }
            }
            preview.session_renewal = Some(next.clone());
            preview.writes_revoked = true;
            write_agent_state(transaction, &state)
        })
    }

    async fn begin_public_operation(
        &self,
        intent: &PublicOperationIntent,
        blockers: &[OperationId],
        superseded: &[OperationId],
    ) -> Result<BeginPublicOperation> {
        intent
            .validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.to_string()))?;
        self.with_transaction(|transaction| {
            let mut state = load_state_from_connection(transaction)?;
            let local_identity = state.identity.local_identity.clone();
            super::public_action_state::validate(&state, intent)?;
            let mut ordinary = intent.goal().map(|goal| super::ordinary_goal::freeze(&state, goal)).transpose()?;
            let restoring = intent.goal().is_some_and(|goal| matches!(goal.desired,
                crate::protocol::public_operations::ReplicaGoalKind::RestoreRetainedMember { .. }));
            if restoring {
                let goal = intent.goal().unwrap();
                let (old_pid, old_lifetime) = super::ordinary_goal::retained::validate_restore(&state, goal)?;
                if !self.owns_process(&intent.process_session_id)
                    || old_lifetime == super::ordinary_goal::retained::process_lifetime_id()
                {
                    return Err(crate::host::HostError::CommandRejected(
                        "retained restoration requires a different real process with reacquired exact storage locks".into(),
                    ));
                }
                if state.public_v1_operation_state.as_ref().is_some_and(|preview|
                    preview.operations.contains_key(&intent.operation_id))
                {
                    let existing = state.public_v1_operation_state.as_ref().unwrap()
                        .operations.get(&intent.operation_id).unwrap();
                    super::ordinary_goal::retained::validate_receipt(&state, existing)?;
                } else {
                    let crate::protocol::public_operations::ReplicaGoalKind::RestoreRetainedMember { qualification }
                        = &goal.desired else { unreachable!() };
                    ordinary.as_mut().unwrap().retained = Some(super::state::RetainedMemberReceipt {
                        qualification: qualification.as_ref().clone(),
                        successor_process: goal.scope.process_session_id.clone(),
                        successor_creation: goal.scope.creation_id.clone(),
                        owner_process_id: std::process::id(),
                        predecessor_process_id: old_pid,
                        predecessor_process_lifetime: old_lifetime,
                        owner_process_lifetime: super::ordinary_goal::retained::process_lifetime_id().to_owned(),
                    });
                }
            }
            let recovery_cause = self.owns_process(&intent.process_session_id)
                .then(|| super::ordinary_goal::retained::recovery_cause(&state, intent, std::process::id()))
                .flatten();
            let retained_lineage = super::ordinary_goal::retained::restored_process(&state, &intent.process_session_id);
            let physically_superseded = if restoring {
                state.public_v1_operation_state.as_ref().unwrap().operations.values()
                    .filter(|record| super::ordinary_goal::retained::supersedes_fault(
                        &state, intent.goal().unwrap(), record))
                    .map(|record| record.intent.operation_id.clone())
                    .collect::<Vec<_>>()
            } else { Vec::new() };
            let preview = state.public_v1_operation_state.as_mut().ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "public-operation preview is not enabled for this store".into(),
                )
            })?;
            if preview.identity != intent.identity {
                return Err(crate::host::HostError::IdentityMismatch(
                    "public-operation preview identity changed".into(),
                ));
            }
            if let Some(existing) = preview.operations.get(&intent.operation_id) {
                if &existing.intent != intent {
                    return Err(crate::host::HostError::DurableEffectConflict(
                        "public-operation ID was reused with changed input".into(),
                    ));
                }
                return Ok(match existing.stage {
                    PublicOperationStage::WaitingForContainment => {
                        BeginPublicOperation::Waiting(existing.clone())
                    }
                    PublicOperationStage::Ready
                    | PublicOperationStage::Running
                    | PublicOperationStage::RetryPending
                    | PublicOperationStage::CallbackApplied => {
                        BeginPublicOperation::Pending(existing.clone())
                    }
                    PublicOperationStage::ContainmentPending => {
                        BeginPublicOperation::Waiting(existing.clone())
                    }
                    PublicOperationStage::Completed => {
                        BeginPublicOperation::Completed(existing.clone())
                    }
                });
            }

            if intent.goal().is_some()
                && preview.terminal
                && !restoring && !retained_lineage
                && !super::ordinary_goal::terminal_for_process(preview, &intent.process_session_id)
            {
                preview.terminal = false;
            }
            let mut exact_blockers = std::collections::BTreeSet::new();
            if !intent.class.is_terminal()
                && preview
                    .operations
                    .values()
                    .any(|record| {
                        record.stage != PublicOperationStage::Completed
                            && record.intent.process_session_id != intent.process_session_id
                    })
            {
                let renewal_authorized = preview.session_renewal.as_ref().is_some_and(|renewal| {
                    let replacement_matches =
                        renewal.required.replacement.as_ref().is_some_and(|replacement| {
                            replacement.identity == state.identity.local_identity
                                && replacement.process_session_id == intent.process_session_id
                        });
                    let history_matches = preview
                        .history
                        .as_ref()
                        .is_some_and(|history| renewal.required.history == *history);
                    let staged_authority = match (
                        renewal.stage,
                        intent.program.as_ref(),
                    ) {
                        (
                            crate::protocol::public_operations::PublicSessionRenewalStage::
                                ReplacementRolePending,
                            Some(crate::protocol::public_operations::PublicOperationProgram::Open {
                                replica,
                                existing: true,
                            }),
                        ) => *replica == state.identity.local_identity,
                        (
                            crate::protocol::public_operations::PublicSessionRenewalStage::
                                ReplacementRolePending,
                            Some(crate::protocol::public_operations::PublicOperationProgram::Role {
                                epoch,
                                role,
                            }),
                        ) => {
                            *epoch == renewal.required.renewal_epoch
                                && *role == renewal.required.predecessor.role
                        }
                        (
                            crate::protocol::public_operations::PublicSessionRenewalStage::
                                ConfigurationPending,
                            Some(crate::protocol::public_operations::PublicOperationProgram::Epoch {
                                epoch,
                            }),
                        ) => *epoch == renewal.required.renewal_epoch,
                        (
                            crate::protocol::public_operations::PublicSessionRenewalStage::
                                ConfigurationPending,
                            Some(crate::protocol::public_operations::PublicOperationProgram::Role {
                                epoch,
                                role,
                            }),
                        ) => {
                            *epoch == renewal.required.renewal_epoch
                                && renewal.required.fresh_configuration.members.iter().any(
                                    |member| {
                                        member.identity == state.identity.local_identity
                                            && member.role == *role
                                    },
                                )
                        }
                        (
                            crate::protocol::public_operations::PublicSessionRenewalStage::
                                ConfigurationPending,
                            Some(
                                crate::protocol::public_operations::PublicOperationProgram::
                                    Configuration(configuration),
                            ),
                        ) => configuration.current == renewal.required.fresh_configuration,
                        _ => false,
                    };
                    replacement_matches
                        && history_matches
                        && (renewal.stage
                            == crate::protocol::public_operations::PublicSessionRenewalStage::
                                Authorized
                            || staged_authority)
                });
                let fresh_ordinary_open = intent.goal().is_some_and(|goal| {
                    matches!(goal.desired, crate::protocol::public_operations::ReplicaGoalKind::Discover
                        | crate::protocol::public_operations::ReplicaGoalKind::Prepare { .. })
                        && preview.history.as_ref() == Some(&goal.scope.history)
                        && preview.storage_incarnation.as_ref() == Some(&goal.scope.storage)
                });
                if !renewal_authorized && !fresh_ordinary_open && !restoring {
                    return Err(crate::host::HostError::IdentityMismatch(
                        "fresh session cannot resume predecessor public operations".into(),
                    ));
                }
                if !fresh_ordinary_open && !restoring {
                preview.operations.clear();
                preview.current_operation = None;
                preview.current_topology_operation = None;
                preview.active_builds.clear();
                preview.build_records.clear();
                preview.absent_builds.clear();
                preview.writes_revoked = false;
                }
            }
            if !intent.class.is_terminal() && !preview.history_barriers.is_empty() {
                return Err(crate::host::HostError::CommandRejected(
                    "unresolved history-admission barrier".into(),
                ));
            }
            use crate::protocol::public_operations::PublicOperationProgram;
            if intent.identity.is_public_v1() {
                let history = preview.history.as_ref().ok_or_else(|| {
                    crate::host::HostError::CommandRejected(
                        "public-v1 operation requires initialized history".into(),
                    )
                })?;
                if let Some(input) = &intent.lifecycle
                    && input.possible_data_loss
                        == crate::protocol::public_operations::PossibleDataLossIntent::NotPossible
                        && (input.current.epoch.data_loss_number != history.data_loss_number
                            || input
                                .previous
                                .as_ref()
                                .is_some_and(|previous| {
                                    previous.epoch.data_loss_number != history.data_loss_number
                                }))
                {
                    return Err(crate::host::HostError::CommandRejected(
                        "ordinary public-v1 lifecycle changed the history data-loss number".into(),
                    ));
                }
                if let Some(program) = &intent.program {
                    program
                        .validate_history(history)
                        .map_err(|message| {
                            crate::host::HostError::CommandRejected(message.into())
                        })?;
                }
            }
            match &intent.program {
                Some(PublicOperationProgram::Build(build)) => {
                    let source_storage = if intent.identity.is_public_v1() {
                        Some(preview.storage_incarnation.clone().ok_or_else(|| {
                            crate::host::HostError::CommandRejected(
                                "public-v1 build lacks source storage incarnation".into(),
                            )
                        })?)
                    } else {
                        None
                    };
                    let predecessor = preview
                        .active_builds
                        .get(&build.replica.replica_id)
                        .filter(|active| {
                            (active.replica != build.replica
                                || active.process_session_id != build.process_session_id
                                || active.target_storage != build.target_storage
                                || active.replication_address != build.replication_address)
                                && build_was_promoted(preview, active)
                        })
                        .cloned();
                    if let Some(predecessor) = predecessor {
                        // Promotion transfers ownership to the configuration. A fresh
                        // incarnation may retire that idle slot, not an unfinished Build.
                        preview.active_builds.remove(&build.replica.replica_id);
                        preview.absent_builds.insert(predecessor.attempt.clone());
                        if predecessor.target_storage != build.target_storage
                            && !preview
                            .retired_storage_incarnations
                            .contains(&predecessor.target_storage)
                        {
                            preview
                                .retired_storage_incarnations
                                .push(predecessor.target_storage);
                        }
                        preview
                            .build_records
                            .get_mut(&predecessor.attempt)
                            .expect("promoted build record checked")
                            .retirement = PublicV1BuildRetirement::Retired;
                    }
                    if preview
                        .retired_storage_incarnations
                        .contains(&build.target_storage)
                        || preview.active_builds.contains_key(&build.replica.replica_id)
                        || preview.operations.values().filter(|record| {
                            preview.current_topology_operation.as_ref().map_or_else(
                                || {
                                    preview.current_operation.as_ref()
                                        == Some(&record.intent.operation_id)
                                        && (record.intent.lifecycle.is_some()
                                            || matches!(
                                                record.intent.program,
                                                Some(
                                                    PublicOperationProgram::Configuration(_)
                                                        | PublicOperationProgram::CatchUp {
                                                            ..
                                                        }
                                                        | PublicOperationProgram::Swap { .. }
                                                )
                                            ))
                                },
                                |operation_id| operation_id == &record.intent.operation_id,
                            )
                                || (matches!(
                                    record.intent.program,
                                    Some(PublicOperationProgram::Swap { .. })
                                ) && record.stage != PublicOperationStage::Completed
                                    && record.superseded_by.is_none())
                        }).any(|record| {
                            let contains = |configuration: &crate::protocol::types::ConfigurationDescriptor| {
                                configuration
                                    .members
                                    .iter()
                                    .any(|member| member.identity.replica_id == build.replica.replica_id)
                            };
                            record.intent.lifecycle.as_ref().is_some_and(|input| {
                                contains(&input.current)
                                    || input.previous.as_ref().is_some_and(contains)
                            }) || record.intent.program.as_ref().is_some_and(|program| match program {
                                PublicOperationProgram::Configuration(configuration)
                                | PublicOperationProgram::CatchUp { configuration, .. } => {
                                    contains(&configuration.current)
                                        || configuration.previous.as_ref().is_some_and(contains)
                                }
                                PublicOperationProgram::Swap {
                                    starting,
                                    refreshed,
                                    ..
                                } => {
                                    contains(&starting.current)
                                        || starting.previous.as_ref().is_some_and(contains)
                                        || contains(&refreshed.current)
                                        || refreshed.previous.as_ref().is_some_and(contains)
                                }
                                _ => false,
                            })
                        })
                    {
                        return Err(crate::host::HostError::CommandRejected(
                            "build target is active or has an unretired attempt".into(),
                        ));
                    }
                    preview
                        .active_builds
                        .insert(build.replica.replica_id, build.clone());
                    if intent.identity.is_public_v1() {
                        let build_record = PublicV1BuildRecord {
                            operation_id: intent.operation_id.clone(),
                            build_id: build.attempt.clone(),
                            admission_revision: intent.revision,
                            history: build.history.clone().ok_or_else(|| {
                                crate::host::HostError::CommandRejected(
                                    "public-v1 build lacks exact history".into(),
                                )
                            })?,
                            epoch: build.epoch,
                            source: local_identity.clone(),
                            source_storage: source_storage
                                .expect("public-v1 source storage validated"),
                            source_session: intent.process_session_id.clone(),
                            target: build.replica.clone(),
                            target_storage: build.target_storage.clone(),
                            target_session: build.process_session_id.clone(),
                            target_address: build.replication_address.clone(),
                            retirement: PublicV1BuildRetirement::Retain,
                            result: None,
                        };
                        build_record.validate().map_err(|message| {
                            crate::host::HostError::CommandRejected(message.into())
                        })?;
                        preview
                            .build_records
                            .insert(intent.operation_id.clone(), build_record);
                    }
                }
                Some(PublicOperationProgram::Remove(build)) => {
                    if !preview
                        .active_builds
                        .get(&build.replica.replica_id)
                        .is_some_and(|active| active.same_attempt_as(build))
                    {
                        return Err(crate::host::HostError::CommandRejected(
                            "removal does not name the exact active attempt".into(),
                        ));
                    }
                }
                Some(PublicOperationProgram::Swap { starting, .. }) => {
                    let primary_completed = preview.operations.values().any(|record| {
                        record.stage == PublicOperationStage::Completed
                            && record.disposition == Some(PublicOperationDisposition::Succeeded)
                            && (record.intent.lifecycle.as_ref().is_some_and(|input| {
                                input.recipe
                                    != crate::protocol::public_operations::PublicLifecycleRecipe::SecondaryEpochAdvance
                            }) || matches!(
                                record.intent.program,
                                Some(PublicOperationProgram::Role {
                                    role: ReplicaRole::Primary,
                                    ..
                                })
                            ))
                    });
                    if !primary_completed
                        || starting.current.primary_id != state.identity.local_identity.replica_id
                    {
                        return Err(crate::host::HostError::CommandRejected(
                            "swap requires completed local primary authority".into(),
                        ));
                    }
                }
                Some(PublicOperationProgram::Open { replica, .. })
                    if replica != &state.identity.local_identity =>
                {
                    return Err(crate::host::HostError::IdentityMismatch(
                        "open target is not the local replica".into(),
                    ));
                }
                Some(PublicOperationProgram::Open { .. }) => {
                    // Native BuildIdle membership is process-local. A completed
                    // predecessor-process build is not a live session in this
                    // newly opened owner. Retire that logical slot, not the
                    // target's service storage or acknowledged application data.
                    let predecessors = preview.build_records.values()
                        .filter(|record| {
                            record.source_session != intent.process_session_id
                                && record.retirement == PublicV1BuildRetirement::Retain
                                && record.result == Some(crate::protocol::public_operations::PublicV1OperationResult::Succeeded)
                        })
                        .map(|record| (record.operation_id.clone(), record.target.replica_id, record.build_id.clone()))
                        .collect::<Vec<_>>();
                    for (operation, target, build) in predecessors {
                        if preview.active_builds.get(&target).is_some_and(|active| active.attempt == build) {
                            preview.active_builds.remove(&target);
                        }
                        preview.absent_builds.insert(build);
                        preview.build_records.get_mut(&operation).expect("captured predecessor build")
                            .retirement = PublicV1BuildRetirement::Retired;
                    }
                }
                _ => {}
            }
            if let Some(input) = &intent.lifecycle {
                if intent.identity.is_public_v1()
                    && (preview.history_initialization_id.is_none()
                        || preview.history.is_none()
                        || preview.storage_incarnation.is_none())
                {
                    return Err(crate::host::HostError::CommandRejected(
                        "public-v1 lifecycle requires initialized history and storage".into(),
                    ));
                }
                if input.replica != state.identity.local_identity {
                    return Err(crate::host::HostError::IdentityMismatch(
                        "lifecycle target does not match local replica".into(),
                    ));
                }
                if preview.operations.values().any(|record| {
                    record.intent.lifecycle.is_some()
                        && record.stage != PublicOperationStage::Completed
                        && record.intent.process_session_id != intent.process_session_id
                }) {
                    return Err(crate::host::HostError::IdentityMismatch(
                        "fresh session cannot resume predecessor lifecycle authority".into(),
                    ));
                }
                if preview
                    .operations
                    .values()
                    .filter_map(|record| record.intent.lifecycle.as_ref())
                    .any(|previous| previous.epoch >= input.epoch)
                {
                    return Err(crate::host::HostError::CommandRejected(
                        "lifecycle epoch does not exceed retained epoch".into(),
                    ));
                }
                if input.possible_data_loss
                    == crate::protocol::public_operations::PossibleDataLossIntent::Possible
                {
                    if intent.identity.is_public_v1() {
                        let recovery = input.data_loss_recovery.as_ref().ok_or_else(|| {
                            crate::host::HostError::CommandRejected(
                                "public-v1 data loss requires typed recovery".into(),
                            )
                        })?;
                        if recovery.attempt.process.data_loss_replay
                            != recovery.authorization.replay_disposition
                        {
                            return Err(crate::host::HostError::CommandRejected(
                                "initial data-loss attempt lacks exact captured replay capability"
                                    .into(),
                            ));
                        }
                        if preview.history.as_ref() != Some(&recovery.authorization.predecessor)
                            || preview.storage_incarnation.as_ref()
                                != Some(&recovery.authorization.provisional_primary)
                        {
                            return Err(crate::host::HostError::IdentityMismatch(
                                "data-loss authorization differs from current history or storage"
                                    .into(),
                            ));
                        }
                        preview.writes_revoked = true;
                        preview.active_builds.clear();
                        preview.build_records.clear();
                        preview.absent_builds.clear();
                        preview.operations.retain(|_, record| {
                            record.stage == PublicOperationStage::Completed
                                && record.disposition == Some(PublicOperationDisposition::Succeeded)
                                && record.intent.process_session_id == intent.process_session_id
                                && record.accepted_storage.as_ref()
                                    == Some(&recovery.authorization.provisional_primary)
                                && matches!(
                                    record.intent.program,
                                    Some(crate::protocol::public_operations::PublicOperationProgram::Open { .. })
                                )
                        });
                        preview.current_topology_operation = None;
                        preview.current_operation = None;
                        preview.data_loss_authorization =
                            Some(recovery.authorization.clone());
                        preview.data_loss_recovery = Some(
                            crate::protocol::public_operations::PublicDataLossRecoveryStatus {
                                authorization_id: recovery
                                    .authorization
                                    .authorization_id
                                    .clone(),
                                attempt: recovery.attempt.clone(),
                                predecessor_history: recovery.authorization.predecessor.clone(),
                                proposed_history: recovery.authorization.proposed.clone(),
                                provisional_primary: recovery
                                    .authorization
                                    .provisional_primary
                                    .clone(),
                                reset_scope: recovery.authorization.reset_scope.clone(),
                                replay_disposition: recovery.authorization.replay_disposition,
                                stage: crate::protocol::public_operations::
                                    PublicDataLossRecoveryStage::Invalidated,
                                observed_progress: None,
                            },
                        );
                    }
                    preview.history_barriers.insert(
                        intent.operation_id.clone(),
                        crate::host::state::HistoryAdmissionBarrier {
                            intent: intent.clone(),
                            outcome: crate::host::state::DataLossOutcome::NotInvoked,
                            retired_by: None,
                        },
                    );
                }
            }
            if matches!(
                intent.class,
                crate::protocol::public_operations::PublicOperationClass::Authority
            ) || intent.class.is_terminal()
            {
                preview.current_operation = Some(intent.operation_id.clone());
            }
            if intent.lifecycle.is_some()
                || matches!(
                    intent.program,
                    Some(
                        PublicOperationProgram::Configuration(_)
                            | PublicOperationProgram::CatchUp { .. }
                    )
                )
            {
                preview.current_topology_operation = Some(intent.operation_id.clone());
            }
            retire_history_barriers(preview, intent);
            if intent.class.is_terminal() {
                preview.writes_revoked = true;
                preview.terminal = true;
            }
            match intent.class {
                crate::protocol::public_operations::PublicOperationClass::TransientFault
                    if state.reported_fault != Some(FaultType::Permanent) =>
                {
                    state.reported_fault = Some(FaultType::Transient);
                }
                crate::protocol::public_operations::PublicOperationClass::PermanentFault => {
                    state.reported_fault = Some(FaultType::Permanent);
                }
                _ => {}
            }
            for blocker in blockers {
                if blocker == &intent.operation_id {
                    return Err(crate::host::HostError::CommandRejected(
                        "public operation cannot block on itself".into(),
                    ));
                }
                let Some(record) = preview.operations.get(blocker) else {
                    return Err(crate::host::HostError::CommandRejected(format!(
                        "public-operation blocker {blocker} is unknown"
                    )));
                };
                if record.stage != PublicOperationStage::Completed && !physically_superseded.contains(blocker) {
                    exact_blockers.insert(blocker.clone());
                }
            }
            for id in &physically_superseded {
                let predecessor = preview.operations.get_mut(id).unwrap();
                if predecessor.superseded_by.as_ref().is_some_and(|existing| existing != &intent.operation_id) {
                    return Err(crate::host::HostError::DurableEffectConflict(
                        "excluded predecessor already has another scoped successor".into(),
                    ));
                }
                predecessor.superseded_by = Some(intent.operation_id.clone());
                // Physical custody settles this dependency, never the old
                // disposition, lifecycle, Cleanup outcome or containment.
            }
            for operation_id in superseded {
                let blocked = preview.operations.get_mut(operation_id).ok_or_else(|| {
                    crate::host::HostError::CommandRejected(format!(
                        "superseded public operation {operation_id} disappeared"
                    ))
                })?;
                if blocked
                    .superseded_by
                    .as_ref()
                    .is_some_and(|existing| existing != &intent.operation_id)
                {
                    return Err(crate::host::HostError::DurableEffectConflict(format!(
                        "public operation {operation_id} was already superseded"
                    )));
                }
                blocked.superseded_by = Some(intent.operation_id.clone());
            }
            let stage = if exact_blockers.is_empty() {
                PublicOperationStage::Ready
            } else {
                PublicOperationStage::WaitingForContainment
            };
            let record = PublicOperationRecord {
                intent: intent.clone(),
                ordinary,
                attempt: 1,
                attempt_results: Default::default(),
                retry_failure: None,
                accepted_history: preview.history.clone(),
                accepted_storage: preview.storage_incarnation.clone(),
                stage,
                disposition: None,
                containment: if recovery_cause.is_some() {
                    PublicOperationContainment::Pending
                } else {
                    PublicOperationContainment::NotRequired
                },
                superseded_by: None,
                blockers: exact_blockers,
                lifecycle: super::state::PublicLifecycleRecord {
                    in_flight: None,
                    outcomes: recovery_cause.into_iter()
                        .map(|notice| PublicInstructionOutcome::RecoveryExitRequired(Box::new(notice)))
                        .collect(),
                },
            };
            preview
                .operations
                .insert(intent.operation_id.clone(), record.clone());
            write_agent_state(transaction, &state)?;
            Ok(if stage == PublicOperationStage::Ready {
                BeginPublicOperation::Ready(record)
            } else {
                BeginPublicOperation::Waiting(record)
            })
        })
    }

    async fn renew_public_v1_data_loss_attempt(
        &self,
        intent: &PublicOperationIntent,
    ) -> Result<RenewPublicDataLossAttempt> {
        intent
            .validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.to_string()))?;
        let input = intent.lifecycle.as_ref().ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "public-v1 data-loss renewal requires lifecycle input".into(),
            )
        })?;
        let candidate = input.data_loss_recovery.as_ref().ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "public-v1 data-loss renewal requires typed recovery".into(),
            )
        })?;
        if !intent.identity.is_public_v1()
            || input.possible_data_loss != PossibleDataLossIntent::Possible
        {
            return Err(crate::host::HostError::CommandRejected(
                "public-v1 data-loss renewal requires protocol 11 possible-loss authority".into(),
            ));
        }

        self.with_transaction(|transaction| {
            let mut state = load_state_from_connection(transaction)?;
            let preview = state.public_v1_operation_state.as_mut().ok_or_else(|| {
                crate::host::HostError::CommandRejected("replacement store required".into())
            })?;
            if preview.identity != intent.identity {
                return Err(crate::host::HostError::IdentityMismatch(
                    "public-operation preview identity changed".into(),
                ));
            }
            if preview.operations.contains_key(&intent.operation_id) {
                return Err(crate::host::HostError::DurableEffectConflict(
                    "data-loss renewal attempt already exists".into(),
                ));
            }
            let authorization = preview.data_loss_authorization.as_ref().ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "data-loss renewal lacks durable authorization".into(),
                )
            })?;
            let recovery = preview.data_loss_recovery.as_ref().ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "data-loss renewal lacks durable recovery state".into(),
                )
            })?;
            if let Some(session_renewal) = &preview.session_renewal {
                let required = &session_renewal.required;
                let Some(data_loss) = &required.data_loss else {
                    return Err(crate::host::HostError::IdentityMismatch(
                        "data-loss attempt differs from durable session renewal".into(),
                    ));
                };
                let expected_stage = match required
                    .data_loss_decision()
                    .expect("validated data-loss renewal")
                {
                    DataLossRenewalDecision::InvokeCallback => {
                        crate::protocol::public_operations::PublicSessionRenewalStage::Authorized
                    }
                    DataLossRenewalDecision::RetireWithoutCallback => {
                        crate::protocol::public_operations::PublicSessionRenewalStage::Retired
                    }
                    DataLossRenewalDecision::Terminal => {
                        return Err(crate::host::HostError::Corrupt(
                            "terminal data-loss session renewal was persisted".into(),
                        ));
                    }
                };
                if session_renewal.stage != expected_stage
                    || data_loss.authorization != candidate.authorization
                    || data_loss.predecessor_attempt != recovery.attempt
                    || data_loss.replacement_attempt != candidate.attempt
                {
                    return Err(crate::host::HostError::CommandRejected(
                        "data-loss callback renewal has not crossed exact session barriers".into(),
                    ));
                }
            }
            if authorization != &candidate.authorization
                || recovery.authorization_id != authorization.authorization_id
                || recovery.predecessor_history != authorization.predecessor
                || recovery.proposed_history != authorization.proposed
                || recovery.provisional_primary != authorization.provisional_primary
                || recovery.reset_scope != authorization.reset_scope
                || recovery.replay_disposition != authorization.replay_disposition
                || recovery.stage
                    != crate::protocol::public_operations::PublicDataLossRecoveryStage::
                        CallbackProcessing
            {
                return Err(crate::host::HostError::IdentityMismatch(
                    "data-loss renewal differs from durable logical authorization".into(),
                ));
            }

            let predecessor_id = recovery.attempt.attempt_id.clone();
            let predecessor = preview
                .operations
                .get(&predecessor_id)
                .cloned()
                .ok_or_else(|| {
                    crate::host::HostError::Corrupt(
                        "data-loss renewal predecessor operation is absent".into(),
                    )
                })?;
            let predecessor_barrier =
                preview
                    .history_barriers
                    .get(&predecessor_id)
                    .ok_or_else(|| {
                        crate::host::HostError::Corrupt(
                            "data-loss renewal predecessor barrier is absent".into(),
                        )
                    })?;
            if predecessor.stage != PublicOperationStage::ContainmentPending
                || predecessor.containment != PublicOperationContainment::Pending
                || !matches!(
                    predecessor.disposition,
                    Some(PublicOperationDisposition::Ambiguous(_))
                )
                || predecessor_barrier.outcome != crate::host::state::DataLossOutcome::Ambiguous
                || predecessor_barrier.retired_by.is_some()
                || predecessor.intent.lifecycle.as_ref().and_then(|input| {
                    input
                        .data_loss_recovery
                        .as_ref()
                        .map(|recovery| &recovery.attempt)
                }) != Some(&recovery.attempt)
            {
                return Err(crate::host::HostError::CommandRejected(
                    "data-loss renewal predecessor is not durably contained".into(),
                ));
            }

            if candidate
                .attempt
                .validate_for(
                    authorization,
                    recovery.attempt.process.epoch.configuration_number,
                )
                .is_err()
                || candidate.attempt.attempt_id == recovery.attempt.attempt_id
                || candidate.attempt.process.process_session_id
                    == recovery.attempt.process.process_session_id
            {
                return Ok(RenewPublicDataLossAttempt {
                    decision: DataLossRenewalDecision::Terminal,
                    predecessor,
                    successor: None,
                });
            }

            let decision = crate::protocol::public_operations::DataLossAttempt::renewal_decision(
                authorization,
                &recovery.attempt,
                &candidate.attempt,
                true,
            );
            let successor = PublicOperationRecord {
                ordinary: None,
                intent: intent.clone(),
                attempt: 1,
                attempt_results: Default::default(),
                retry_failure: None,
                accepted_history: preview.history.clone(),
                accepted_storage: preview.storage_incarnation.clone(),
                stage: if decision == DataLossRenewalDecision::InvokeCallback {
                    PublicOperationStage::Ready
                } else {
                    PublicOperationStage::Completed
                },
                disposition: (decision != DataLossRenewalDecision::InvokeCallback)
                    .then_some(PublicOperationDisposition::Cancelled),
                containment: if decision == DataLossRenewalDecision::InvokeCallback {
                    PublicOperationContainment::NotRequired
                } else {
                    PublicOperationContainment::Complete
                },
                superseded_by: None,
                blockers: Default::default(),
                lifecycle: Default::default(),
            };

            let predecessor = preview
                .operations
                .get_mut(&predecessor_id)
                .expect("validated predecessor operation");
            predecessor.stage = PublicOperationStage::Completed;
            predecessor.containment = PublicOperationContainment::Complete;
            predecessor.superseded_by = Some(intent.operation_id.clone());
            let predecessor = predecessor.clone();

            if decision == DataLossRenewalDecision::InvokeCallback {
                preview.history_barriers.remove(&predecessor_id);
                preview.history_barriers.insert(
                    intent.operation_id.clone(),
                    crate::host::state::HistoryAdmissionBarrier {
                        intent: intent.clone(),
                        outcome: crate::host::state::DataLossOutcome::NotInvoked,
                        retired_by: None,
                    },
                );
                preview.data_loss_recovery = Some(
                    crate::protocol::public_operations::PublicDataLossRecoveryStatus {
                        authorization_id: authorization.authorization_id.clone(),
                        attempt: candidate.attempt.clone(),
                        predecessor_history: authorization.predecessor.clone(),
                        proposed_history: authorization.proposed.clone(),
                        provisional_primary: authorization.provisional_primary.clone(),
                        reset_scope: authorization.reset_scope.clone(),
                        replay_disposition: authorization.replay_disposition,
                        stage: crate::protocol::public_operations::
                            PublicDataLossRecoveryStage::Invalidated,
                        observed_progress: None,
                    },
                );
            } else {
                preview
                    .history_barriers
                    .get_mut(&predecessor_id)
                    .expect("validated predecessor barrier")
                    .retired_by = Some(intent.operation_id.clone());
                let recovery = preview
                    .data_loss_recovery
                    .as_mut()
                    .expect("validated durable recovery");
                recovery.attempt = candidate.attempt.clone();
                recovery.stage = match decision {
                    DataLossRenewalDecision::RetireWithoutCallback => {
                        crate::protocol::public_operations::PublicDataLossRecoveryStage::Retired
                    }
                    DataLossRenewalDecision::Terminal => crate::protocol::public_operations::
                        PublicDataLossRecoveryStage::ReplacementRequired,
                    DataLossRenewalDecision::InvokeCallback => unreachable!(),
                };
                recovery.observed_progress = None;
            }
            preview.current_operation = Some(intent.operation_id.clone());
            preview.current_topology_operation = Some(intent.operation_id.clone());
            preview
                .operations
                .insert(intent.operation_id.clone(), successor.clone());
            write_agent_state(transaction, &state)?;
            Ok(RenewPublicDataLossAttempt {
                decision,
                predecessor,
                successor: Some(successor),
            })
        })
    }

    async fn advance_public_operation(
        &self,
        operation_id: &OperationId,
        expected_revision: u64,
        expected_process_session: &crate::protocol::types::ProcessSessionId,
        expected: PublicOperationStage,
        next: PublicOperationStage,
        disposition: Option<PublicOperationDisposition>,
    ) -> Result<PublicOperationRecord> {
        self.with_transaction(|transaction| {
            if !valid_public_operation_transition(expected, next) {
                return Err(crate::host::HostError::CommandRejected(format!(
                    "invalid public-operation stage transition {expected:?} -> {next:?}"
                )));
            }
            let mut state = load_state_from_connection(transaction)?;
            if matches!(next, PublicOperationStage::CallbackApplied | PublicOperationStage::Completed)
                && let Some(record) = state.public_v1_operation_state.as_ref()
                    .and_then(|preview| preview.operations.get(operation_id))
                && record.intent.goal().is_some_and(|goal| matches!(goal.desired,
                    crate::protocol::public_operations::ReplicaGoalKind::RestoreRetainedMember { .. }))
                && (disposition == Some(PublicOperationDisposition::Succeeded)
                    || (disposition.is_none() && record.disposition == Some(PublicOperationDisposition::Succeeded)))
            {
                super::ordinary_goal::retained::validate_receipt(&state, record)?;
                if !self.owns_process(expected_process_session) {
                    return Err(crate::host::HostError::CommandRejected(
                        "retained completion lost exclusive physical custody".into(),
                    ));
                }
            }
            let contained_transient_reopen = next == PublicOperationStage::Completed
                && disposition == Some(PublicOperationDisposition::Succeeded)
                && state.public_v1_operation_state.as_ref()
                    .and_then(|preview| preview.current_operation.as_ref()) == Some(operation_id)
                && crate::host::operation::contained_transient_reopen(&state, expected_process_session);
            let preview = state.public_v1_operation_state.as_mut().ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "public-operation preview is not enabled for this store".into(),
                )
            })?;
            let record = preview.operations.get_mut(operation_id).ok_or_else(|| {
                crate::host::HostError::CommandRejected(format!(
                    "public operation {operation_id} is unknown"
                ))
            })?;
            if record.stage != expected {
                return Err(crate::host::HostError::StaleEffectCompletion(format!(
                    "public operation {operation_id} is at {:?}, expected {expected:?}",
                    record.stage
                )));
            }
            if next == PublicOperationStage::CallbackApplied
                && disposition == Some(PublicOperationDisposition::Succeeded)
                && !crate::host::public_lifecycle::complete(record)
            {
                return Err(crate::host::HostError::DurableEffectConflict(
                    "lifecycle callback sequence is incomplete".into(),
                ));
            }
            if record.intent.revision != expected_revision
                || &record.intent.process_session_id != expected_process_session
            {
                return Err(crate::host::HostError::StaleEffectCompletion(format!(
                    "public operation {operation_id} revision or process session is stale"
                )));
            }
            if record.superseded_by.is_some()
                && matches!(
                    next,
                    PublicOperationStage::CallbackApplied
                        | PublicOperationStage::Running
                        | PublicOperationStage::Ready
                )
            {
                return Err(crate::host::HostError::StaleEffectCompletion(format!(
                    "public operation {operation_id} was superseded"
                )));
            }
            if record.superseded_by.is_some()
                && expected == PublicOperationStage::CallbackApplied
                && next == PublicOperationStage::Completed
                && disposition.is_none()
            {
                return Err(crate::host::HostError::StaleEffectCompletion(format!(
                    "public operation {operation_id} was superseded after callback application"
                )));
            }
            record.stage = next;
            if let Some(disposition) = disposition {
                record.disposition = Some(disposition);
            }
            if next == PublicOperationStage::Completed
                && record.disposition == Some(PublicOperationDisposition::Succeeded)
                && record.intent.goal().is_some_and(|goal| matches!(
                    goal.desired, crate::protocol::public_operations::ReplicaGoalKind::Remove { .. }
                ))
                && let Some(ordinary) = record.ordinary.as_mut()
            {
                ordinary.logically_retired = true;
            }
            record.containment = match next {
                PublicOperationStage::ContainmentPending => PublicOperationContainment::Pending,
                PublicOperationStage::Completed if record.lifecycle.outcomes.iter().any(|outcome| {
                    matches!(outcome, PublicInstructionOutcome::RecoveryExitRequired(_))
                }) => record.containment,
                PublicOperationStage::Completed => PublicOperationContainment::Complete,
                _ => record.containment,
            };
            if next == PublicOperationStage::Completed && record.disposition.is_none() {
                return Err(crate::host::HostError::CommandRejected(
                    "completed public operation requires a disposition".into(),
                ));
            }
            if matches!(
                next,
                PublicOperationStage::ContainmentPending | PublicOperationStage::Completed
            ) && let Some(disposition) = record.disposition.clone()
            {
                record.attempt_results.entry(record.attempt).or_insert_with(|| {
                    crate::host::state::PublicOperationAttemptResult {
                        stage: next,
                        disposition,
                        lifecycle: record.lifecycle.clone(),
                        retry_failure: record.retry_failure.clone(),
                        containment: record.containment,
                        superseded_by: record.superseded_by.clone(),
                        blockers: record.blockers.clone(),
                    }
                });
            }
            if next == PublicOperationStage::Ready {
                record.blockers.clear();
                record.lifecycle.in_flight = None;
            }
            let result = record.clone();
            if let Some(build_record) = preview.build_records.get_mut(operation_id)
                && matches!(
                    result.intent.program,
                    Some(crate::protocol::public_operations::PublicOperationProgram::Build(_))
                )
            {
                build_record.result = public_v1_build_result(&result);
                if matches!(
                    build_record.result,
                    Some(
                        crate::protocol::public_operations::PublicV1OperationResult::Failed(_)
                            | crate::protocol::public_operations::PublicV1OperationResult::Ambiguous(
                                _
                            )
                            | crate::protocol::public_operations::PublicV1OperationResult::Cancelled
                            | crate::protocol::public_operations::PublicV1OperationResult::RebuildRequired(
                                _
                            )
                    )
                ) {
                    build_record.retirement = PublicV1BuildRetirement::Required;
                }
                build_record.validate().map_err(|message| {
                    crate::host::HostError::DurableEffectConflict(message.into())
                })?;
            }
            if contained_transient_reopen {
                preview.terminal = false;
                state.reported_fault = None;
            }
            write_agent_state(transaction, &state)?;
            Ok(result)
        })
    }

    async fn record_public_retry_failure(
        &self,
        intent: &PublicOperationIntent,
        attempt: u64,
        failure: &crate::host::state::PublicCallbackFailure,
    ) -> Result<PublicOperationRecord> {
        self.with_transaction(|transaction| {
            let mut state = load_state_from_connection(transaction)?;
            let preview = state.public_v1_operation_state.as_mut().ok_or_else(|| {
                crate::host::HostError::CommandRejected("replacement store required".into())
            })?;
            let record = preview
                .operations
                .get_mut(&intent.operation_id)
                .ok_or_else(|| {
                    crate::host::HostError::CommandRejected("unknown retry operation".into())
                })?;
            if &record.intent != intent
                || attempt == 0
                || record.attempt != attempt
                || record.stage != PublicOperationStage::Running
                || record.superseded_by.is_some()
                || record.lifecycle.in_flight != Some(failure.instruction)
                || crate::host::public_lifecycle::operation_instructions(intent)
                    .get(record.lifecycle.outcomes.len())
                    != Some(&failure.instruction)
                || !failure.family.allows(failure.instruction)
                || failure.error.is_empty()
                || intent.class.is_terminal()
                || intent.lifecycle.as_ref().is_some_and(|input| {
                    input.possible_data_loss == PossibleDataLossIntent::Possible
                })
                || record
                    .lifecycle
                    .outcomes
                    .iter()
                    .any(|outcome| matches!(outcome, PublicInstructionOutcome::RebuildRequired(_)))
                || record.attempt_results.contains_key(&attempt)
            {
                return Err(crate::host::HostError::StaleEffectCompletion(
                    "callback failure lacks an exact retryable selected boundary".into(),
                ));
            }
            let disposition = PublicOperationDisposition::Failed(failure.error.clone());
            record.attempt_results.insert(
                attempt,
                crate::host::state::PublicOperationAttemptResult {
                    stage: PublicOperationStage::RetryPending,
                    disposition: disposition.clone(),
                    lifecycle: record.lifecycle.clone(),
                    retry_failure: Some(failure.clone()),
                    containment: record.containment,
                    superseded_by: record.superseded_by.clone(),
                    blockers: record.blockers.clone(),
                },
            );
            record.stage = PublicOperationStage::RetryPending;
            record.disposition = Some(disposition);
            record.retry_failure = Some(failure.clone());
            let result = record.clone();
            write_agent_state(transaction, &state)?;
            Ok(result)
        })
    }

    async fn resume_public_operation(
        &self,
        intent: &PublicOperationIntent,
        attempt: u64,
    ) -> Result<PublicOperationRecord> {
        self.with_transaction(|transaction| {
            let mut state = load_state_from_connection(transaction)?;
            super::public_action_state::validate(&state, intent)?;
            let preview = state.public_v1_operation_state.as_mut().ok_or_else(|| {
                crate::host::HostError::CommandRejected("replacement store required".into())
            })?;
            let record = preview.operations.get(&intent.operation_id).ok_or_else(|| {
                crate::host::HostError::CommandRejected("unknown retry operation".into())
            })?;
            if &record.intent != intent
                || attempt == 0
                || record.attempt != attempt
                || record.stage != PublicOperationStage::RetryPending
                || record.superseded_by.is_some()
                || record.accepted_history != preview.history
                || record.accepted_storage != preview.storage_incarnation
                || preview.terminal
                || !preview.history_barriers.is_empty()
                || !record.blockers.is_empty()
                || record.retry_failure.as_ref().is_none_or(|failure| {
                    record.lifecycle.in_flight != Some(failure.instruction)
                        || !failure.family.allows(failure.instruction)
                        || record.disposition != Some(PublicOperationDisposition::Failed(failure.error.clone()))
                })
                || record.attempt_results.get(&attempt).is_none_or(|result| {
                    result.stage != PublicOperationStage::RetryPending
                        || Some(result.disposition.clone()) != record.disposition
                        || result.lifecycle != record.lifecycle
                        || result.retry_failure != record.retry_failure
                })
                || (intent.class == crate::protocol::public_operations::PublicOperationClass::Authority
                    && preview.current_operation.as_ref() != Some(&intent.operation_id))
                || preview.operations.values().any(|other| {
                    other.intent.revision > intent.revision
                        && matches!(other.intent.class,
                            crate::protocol::public_operations::PublicOperationClass::Authority
                                | crate::protocol::public_operations::PublicOperationClass::PlannedSwap)
                })
            {
                return Err(crate::host::HostError::StaleEffectCompletion(
                    "retry no longer owns its exact live authority".into(),
                ));
            }
            let record = preview.operations.get_mut(&intent.operation_id).expect("checked operation");
            record.attempt = attempt.checked_add(1).ok_or_else(|| {
                crate::host::HostError::CommandRejected("operation attempt exhausted".into())
            })?;
            record.stage = PublicOperationStage::Ready;
            record.disposition = None;
            record.retry_failure = None;
            record.lifecycle.in_flight = None;
            let result = record.clone();
            write_agent_state(transaction, &state)?;
            Ok(result)
        })
    }

    async fn attach_public_operation(
        &self,
        intent: &PublicOperationIntent,
        owner: &OperationId,
    ) -> Result<PublicOperationRecord> {
        intent
            .validate()
            .map_err(|message| crate::host::HostError::CommandRejected(message.to_string()))?;
        self.with_transaction(|transaction| {
            let mut state = load_state_from_connection(transaction)?;
            if let Some(goal) = intent.goal() {
                super::ordinary_goal::validate_admission(&state, goal)?;
                return Err(crate::host::HostError::CommandRejected(
                    "semantic goals cannot attach to another terminal goal".into(),
                ));
            }
            let preview = state.public_v1_operation_state.as_mut().ok_or_else(|| {
                crate::host::HostError::CommandRejected(
                    "public-operation preview is not enabled for this store".into(),
                )
            })?;
            if preview.identity != intent.identity {
                return Err(crate::host::HostError::IdentityMismatch(
                    "public-operation preview identity changed".into(),
                ));
            }
            let owner_record = preview.operations.get(owner).ok_or_else(|| {
                crate::host::HostError::CommandRejected(format!(
                    "attached public-operation owner {owner} is unknown"
                ))
            })?;
            if owner_record.superseded_by.is_some()
                || matches!(
                    owner_record.disposition,
                    Some(PublicOperationDisposition::Attached(_))
                )
            {
                return Err(crate::host::HostError::StaleEffectCompletion(format!(
                    "attached public-operation owner {owner} is not authoritative"
                )));
            }
            if let Some(existing) = preview.operations.get(&intent.operation_id) {
                if &existing.intent != intent {
                    return Err(crate::host::HostError::DurableEffectConflict(
                        "public-operation ID was reused with changed input".into(),
                    ));
                }
                return Ok(existing.clone());
            }
            let owner_completed = owner_record.stage == PublicOperationStage::Completed;
            let record = PublicOperationRecord {
                ordinary: None,
                intent: intent.clone(),
                attempt: 1,
                attempt_results: Default::default(),
                retry_failure: None,
                accepted_history: owner_record.accepted_history.clone(),
                accepted_storage: owner_record.accepted_storage.clone(),
                stage: if owner_completed {
                    PublicOperationStage::Completed
                } else {
                    PublicOperationStage::WaitingForContainment
                },
                disposition: Some(PublicOperationDisposition::Attached(owner.clone())),
                containment: if owner_completed {
                    PublicOperationContainment::Complete
                } else {
                    PublicOperationContainment::NotRequired
                },
                superseded_by: None,
                blockers: if owner_completed {
                    std::collections::BTreeSet::new()
                } else {
                    std::collections::BTreeSet::from([owner.clone()])
                },
                lifecycle: Default::default(),
            };
            retire_history_barriers(preview, intent);
            preview.current_operation = Some(intent.operation_id.clone());
            if matches!(
                intent.class,
                crate::protocol::public_operations::PublicOperationClass::TransientFault
                    | crate::protocol::public_operations::PublicOperationClass::PermanentFault
            ) {
                preview.writes_revoked = true;
                preview.terminal = true;
                state.reported_fault = Some(match intent.class {
                    crate::protocol::public_operations::PublicOperationClass::TransientFault => {
                        state.reported_fault.unwrap_or(FaultType::Transient)
                    }
                    crate::protocol::public_operations::PublicOperationClass::PermanentFault => {
                        FaultType::Permanent
                    }
                    _ => unreachable!(),
                });
            }
            preview
                .operations
                .insert(intent.operation_id.clone(), record.clone());
            write_agent_state(transaction, &state)?;
            Ok(record)
        })
    }

    async fn public_operation_records(&self) -> Result<Vec<PublicOperationRecord>> {
        let state = self.load_state().await?;
        let preview = state.public_v1_operation_state.ok_or_else(|| {
            crate::host::HostError::CommandRejected(
                "public-operation preview is not enabled for this store".into(),
            )
        })?;
        Ok(preview.operations.into_values().collect())
    }

    async fn public_instruction(
        &self,
        intent: &PublicOperationIntent,
        index: usize,
        instruction: crate::host::state::PublicInstruction,
        outcome: Option<crate::host::state::PublicInstructionOutcome>,
    ) -> Result<()> {
        use crate::host::state::{DataLossOutcome, PublicInstruction, PublicInstructionOutcome};
        self.with_transaction(|transaction| {
            let mut outcome = outcome;
            if let Some(PublicInstructionOutcome::BuiltinNativeEndpoint {
                process_id, process_lifetime, storage_owner, ..
            }) = outcome.as_mut() {
                if *process_id != std::process::id()
                    || process_lifetime.as_str() != super::ordinary_goal::retained::process_lifetime_id()
                {
                    return Err(crate::host::HostError::IdentityMismatch(
                        "builtin native registration returned from another physical process".into(),
                    ));
                }
                // Never trust an input custody claim. Only the actual process
                // store owner can attach its own independently held locks.
                *storage_owner = self.owns_process(&intent.process_session_id)
                    .then(|| intent.process_session_id.clone());
            }
            let mut state = load_state_from_connection(transaction)?;
            if let Some(goal) = intent.goal().filter(|goal| matches!(goal.desired,
                crate::protocol::public_operations::ReplicaGoalKind::RestoreRetainedMember { .. }))
            {
                let record = state.public_v1_operation_state.as_ref()
                    .and_then(|preview| preview.operations.get(&intent.operation_id))
                    .ok_or_else(|| crate::host::HostError::CommandRejected("restoration record disappeared".into()))?;
                super::ordinary_goal::retained::validate_receipt(&state, record)?;
                if !self.owns_process(&goal.scope.process_session_id) {
                    return Err(crate::host::HostError::CommandRejected("restoration lost physical storage custody".into()));
                }
                if matches!(instruction, PublicInstruction::ApplicationRole)
                    && !record.lifecycle.outcomes.iter().any(|outcome| matches!(outcome,
                        PublicInstructionOutcome::RetainedNativeInitialized(scope) if scope.as_ref() == &goal.scope))
                {
                    return Err(crate::host::HostError::CommandRejected("retained application role precedes actual native initialization".into()));
                }
                if matches!(instruction, PublicInstruction::ReplicatorRole)
                    && outcome.as_ref().is_some_and(|outcome| !matches!(outcome,
                        PublicInstructionOutcome::RetainedNativeInitialized(scope) if scope.as_ref() == &goal.scope))
                {
                    return Err(crate::host::HostError::CommandRejected("retained role lacks actual native initialization receipt".into()));
                }
            }
            if let Some(PublicInstructionOutcome::RetainedNativeInitialized(scope)) = &outcome
                && intent.goal().is_none_or(|goal| {
                    !matches!(goal.desired, crate::protocol::public_operations::ReplicaGoalKind::RestoreRetainedMember { .. })
                        || scope.as_ref() != &goal.scope
                })
            {
                return Err(crate::host::HostError::CommandRejected("native retained receipt is outside its selected restoration".into()));
            }
            let preview = state.public_v1_operation_state.as_mut().ok_or_else(|| {
                crate::host::HostError::CommandRejected("replacement store required".into())
            })?;
            let record = preview
                .operations
                .get_mut(&intent.operation_id)
                .ok_or_else(|| {
                    crate::host::HostError::CommandRejected("unknown replacement operation".into())
                })?;
            let ordinary_return = intent.goal().is_some()
                && outcome.is_some()
                && record.lifecycle.in_flight == Some(instruction);
            if &record.intent != intent
                || preview.identity != intent.identity
                || record.stage != PublicOperationStage::Running
                || (record.superseded_by.is_some() && !ordinary_return)
                || ((intent.class
                    == crate::protocol::public_operations::PublicOperationClass::Authority
                    || (intent.class
                        == crate::protocol::public_operations::PublicOperationClass::PlannedSwap
                        && matches!(
                            instruction,
                            PublicInstruction::ProgramEpoch
                                | PublicInstruction::RefreshedConfiguration
                                | PublicInstruction::SecondCatchUp
                                | PublicInstruction::ReplicatorRole
                                | PublicInstruction::ApplicationRole
                        )))
                    && preview.current_operation.as_ref() != Some(&intent.operation_id)
                    && !ordinary_return)
                || record.lifecycle.outcomes.len() != index
                || record.ordinary.as_ref().map_or_else(
                    || crate::host::public_lifecycle::operation_instructions(intent),
                    |ordinary| ordinary.actions.clone(),
                ).get(index)
                    != Some(&instruction)
            {
                return Err(crate::host::HostError::StaleEffectCompletion(
                    "stale or out-of-order public lifecycle instruction".into(),
                ));
            }
            if matches!(
                instruction,
                PublicInstruction::CatchUpConfiguration
                    | PublicInstruction::Access
                    | PublicInstruction::Configuration
                    | PublicInstruction::StartingConfiguration
                    | PublicInstruction::RefreshedConfiguration
                    | PublicInstruction::FirstCatchUp
                    | PublicInstruction::SecondCatchUp
                    | PublicInstruction::Build
            ) && !preview.history_barriers.is_empty()
            {
                return Err(crate::host::HostError::CommandRejected(
                    "history admission is unresolved".into(),
                ));
            }
            match outcome {
                None if record.lifecycle.in_flight.is_none()
                    || (record.lifecycle.in_flight == Some(instruction)
                        && intent.goal().is_none()
                        && crate::host::public_lifecycle::repeatable(instruction)) =>
                {
                    record.lifecycle.in_flight = Some(instruction);
                    if instruction == PublicInstruction::DataLoss {
                        preview
                            .history_barriers
                            .get_mut(&intent.operation_id)
                            .ok_or_else(|| {
                                crate::host::HostError::Corrupt("missing history barrier".into())
                            })?
                            .outcome = DataLossOutcome::Ambiguous;
                        if let Some(recovery) = preview.data_loss_recovery.as_mut() {
                            recovery.stage = crate::protocol::public_operations::
                                PublicDataLossRecoveryStage::CallbackProcessing;
                        }
                    }
                    if matches!(
                        instruction,
                        PublicInstruction::Revoke
                            | PublicInstruction::Abort
                            | PublicInstruction::ReplicatorClose
                    ) {
                        preview.writes_revoked = true;
                    }
                    if instruction == PublicInstruction::Revoke {
                        preview.current_operation = Some(intent.operation_id.clone());
                        preview.current_topology_operation = Some(intent.operation_id.clone());
                    }
                }
                Some(outcome) if record.lifecycle.in_flight == Some(instruction) => {
                    let valid = match (&instruction, &outcome) {
                        (
                            PublicInstruction::ApplicationPrimary
                            | PublicInstruction::ApplicationRole,
                            PublicInstructionOutcome::ApplicationRole(_),
                        ) => true,
                        (
                            PublicInstruction::ReplicatorOpen,
                            PublicInstructionOutcome::Endpoint(_)
                                | PublicInstructionOutcome::BuiltinNativeEndpoint { .. },
                        )
                        | (PublicInstruction::Progress, PublicInstructionOutcome::Progress(_)) => {
                            true
                        }
                        (PublicInstruction::ReplicatorRole,
                            PublicInstructionOutcome::RetainedNativeInitialized(_)) => true,
                        (
                            PublicInstruction::ReplicatorClose,
                            PublicInstructionOutcome::CloseFailure {
                                child: crate::host::state::PublicCloseChild::Replicator,
                                ..
                            },
                        )
                        | (
                            PublicInstruction::ApplicationClose,
                            PublicInstructionOutcome::CloseFailure {
                                child: crate::host::state::PublicCloseChild::Application,
                                ..
                            },
                        ) => true,
                        (
                            PublicInstruction::DataLoss,
                            PublicInstructionOutcome::DataLoss(
                                DataLossOutcome::False
                                | DataLossOutcome::True
                                | DataLossOutcome::Error(_),
                            ),
                        ) => true,
                        (
                            PublicInstruction::ReplicatorRole
                            | PublicInstruction::ApplicationRole
                            | PublicInstruction::ProgramEpoch
                            | PublicInstruction::Configuration
                            | PublicInstruction::StartingConfiguration
                            | PublicInstruction::RefreshedConfiguration
                            | PublicInstruction::FirstCatchUp
                            | PublicInstruction::SecondCatchUp
                            | PublicInstruction::ReplicatorPrimary
                            | PublicInstruction::Epoch
                            | PublicInstruction::ApplicationPrimary
                            | PublicInstruction::DataLoss
                            | PublicInstruction::Progress
                            | PublicInstruction::CurrentConfiguration
                            | PublicInstruction::CatchUpConfiguration
                            | PublicInstruction::CatchUp
                            | PublicInstruction::Access
                            | PublicInstruction::Build,
                            PublicInstructionOutcome::RebuildRequired(_),
                        ) => true,
                        (
                            PublicInstruction::ApplicationPrimary
                            | PublicInstruction::ApplicationRole
                            | PublicInstruction::DataLoss
                            | PublicInstruction::ReplicatorOpen
                            | PublicInstruction::Progress,
                            _,
                        ) => false,
                        (_, PublicInstructionOutcome::Done) => true,
                        _ => false,
                    };
                    if !valid {
                        return Err(crate::host::HostError::DurableEffectConflict(
                            "public instruction outcome does not match callback".into(),
                        ));
                    }
                    if let PublicInstructionOutcome::DataLoss(result) = &outcome {
                        preview
                            .history_barriers
                            .get_mut(&intent.operation_id)
                            .ok_or_else(|| {
                                crate::host::HostError::Corrupt("missing history barrier".into())
                            })?
                            .outcome = result.clone();
                        if let Some(recovery) = preview.data_loss_recovery.as_mut() {
                            recovery.stage = match result {
                                DataLossOutcome::False | DataLossOutcome::True => {
                                    crate::protocol::public_operations::
                                        PublicDataLossRecoveryStage::HistoryPending
                                }
                                DataLossOutcome::Error(_) => {
                                    crate::protocol::public_operations::
                                        PublicDataLossRecoveryStage::Failed
                                }
                                DataLossOutcome::NotInvoked | DataLossOutcome::Ambiguous => {
                                    return Err(crate::host::HostError::DurableEffectConflict(
                                        "invalid completed data-loss outcome".into(),
                                    ));
                                }
                            };
                        }
                    }
                    if let PublicInstructionOutcome::RebuildRequired(_) = &outcome
                        && instruction == PublicInstruction::DataLoss
                        && let Some(recovery) = preview.data_loss_recovery.as_mut()
                    {
                        recovery.stage = crate::protocol::public_operations::
                            PublicDataLossRecoveryStage::ReplacementRequired;
                    }
                    if let PublicInstructionOutcome::Progress(progress) = &outcome
                        && instruction == PublicInstruction::Progress
                        && let Some(recovery) = preview.data_loss_recovery.as_mut()
                    {
                        recovery.observed_progress =
                            Some(crate::protocol::public_operations::PublicProgress {
                                role: crate::protocol::public_operations::PublicProgressRole::
                                    Recovery,
                                current_progress: crate::protocol::public_operations::ProgressQueryResult::Value(*progress),
                                catch_up_capability: crate::protocol::public_operations::ProgressQueryResult::NotReady,
                            });
                    }
                    if matches!(
                        instruction,
                        PublicInstruction::CurrentConfiguration | PublicInstruction::CatchUp
                    ) && let Some(recovery) = preview.data_loss_recovery.as_mut()
                    {
                        recovery.stage = crate::protocol::public_operations::
                            PublicDataLossRecoveryStage::ConfigurationPending;
                    }
                    if instruction == PublicInstruction::HistoryAdmission {
                        let recovery_input = intent
                            .lifecycle
                            .as_ref()
                            .and_then(|input| input.data_loss_recovery.as_ref())
                            .ok_or_else(|| {
                                crate::host::HostError::Corrupt(
                                    "history admission lacks typed recovery".into(),
                                )
                            })?;
                        let recovery = preview.data_loss_recovery.as_mut().ok_or_else(|| {
                            crate::host::HostError::Corrupt(
                                "history admission lacks durable recovery state".into(),
                            )
                        })?;
                        if recovery.authorization_id
                            != recovery_input.authorization.authorization_id
                            || preview.data_loss_authorization.as_ref()
                                != Some(&recovery_input.authorization)
                            || recovery.attempt != recovery_input.attempt
                            || recovery.proposed_history != recovery_input.authorization.proposed
                            || !matches!(
                                recovery.stage,
                                crate::protocol::public_operations::
                                    PublicDataLossRecoveryStage::ConfigurationPending
                            )
                        {
                            return Err(crate::host::HostError::StaleEffectCompletion(
                                "history admission differs from durable recovery".into(),
                            ));
                        }
                        preview.history = Some(recovery.proposed_history.clone());
                        recovery.stage = crate::protocol::public_operations::
                            PublicDataLossRecoveryStage::Admitted;
                        preview.history_barriers.remove(&intent.operation_id);
                    }
                    if instruction == PublicInstruction::Access && !preview.terminal {
                        preview.writes_revoked = false;
                    }
                    if instruction == PublicInstruction::Remove
                        && let Some(
                            crate::protocol::public_operations::PublicOperationProgram::RetireIdleReplica {
                                replica_id,
                            },
                        ) = &intent.program
                    {
                        preview.active_builds.remove(replica_id);
                    }
                    if instruction == PublicInstruction::Remove
                        && let Some(
                            crate::protocol::public_operations::PublicOperationProgram::Remove(
                                build,
                            ),
                        ) = &intent.program
                    {
                        if !preview
                            .active_builds
                            .get(&build.replica.replica_id)
                            .is_some_and(|active| active.same_attempt_as(build))
                        {
                            return Err(crate::host::HostError::StaleEffectCompletion(
                                "removal attempt changed".into(),
                            ));
                        }
                        preview.active_builds.remove(&build.replica.replica_id);
                        preview.absent_builds.insert(build.attempt.clone());
                        if intent.identity.is_public_v1()
                            && !preview
                                .retired_storage_incarnations
                                .contains(&build.target_storage)
                        {
                            preview
                                .retired_storage_incarnations
                                .push(build.target_storage.clone());
                        }
                        if let Some(build_record) = preview
                            .build_records
                            .values_mut()
                            .find(|record| record.build_id == build.attempt)
                        {
                            if build_record.target != build.replica
                                || build_record.target_storage != build.target_storage
                                || build_record.target_session != build.process_session_id
                            {
                                return Err(crate::host::HostError::StaleEffectCompletion(
                                    "build retirement differs from the exact build record".into(),
                                ));
                            }
                            build_record.retirement = PublicV1BuildRetirement::Retired;
                            build_record.validate().map_err(|message| {
                                crate::host::HostError::DurableEffectConflict(message.into())
                            })?;
                        }
                    }
                    if intent.identity.is_public_v1()
                        && matches!(
                            intent.program,
                            Some(
                                crate::protocol::public_operations::PublicOperationProgram::Build(
                                    _
                                )
                            )
                        )
                        && let PublicInstructionOutcome::RebuildRequired(rebuild) = &outcome
                    {
                        let build_record = preview
                            .build_records
                            .get_mut(&intent.operation_id)
                            .ok_or_else(|| {
                                crate::host::HostError::Corrupt(
                                    "public-v1 rebuild outcome lacks its build record".into(),
                                )
                            })?;
                        build_record.result = Some(
                            crate::protocol::public_operations::PublicV1OperationResult::
                                RebuildRequired(rebuild.clone()),
                        );
                        build_record.retirement = PublicV1BuildRetirement::Required;
                        build_record.validate().map_err(|message| {
                            crate::host::HostError::DurableEffectConflict(message.into())
                        })?;
                    }
                    record.lifecycle.outcomes.push(outcome);
                    record.lifecycle.in_flight = None;
                    if instruction == PublicInstruction::Remove
                        && let Some(remote) = intent.goal().and_then(|goal| goal.remote())
                    {
                        for candidate in preview.operations.values_mut() {
                            if candidate.intent.goal().is_some_and(|goal| {
                                matches!(goal.desired, crate::protocol::public_operations::ReplicaGoalKind::Build { .. })
                                    && goal.remote() == Some(remote)
                            }) && let Some(ordinary) = candidate.ordinary.as_mut() {
                                ordinary.logically_retired = true;
                            }
                        }
                    }
                    if instruction == PublicInstruction::ReplicatorOpen && intent.goal().is_some()
                        && let Some(application) = state.application_storage.as_mut()
                    {
                        application.initializing = false;
                    }
                    if instruction == PublicInstruction::Configuration
                        && intent.goal().is_some()
                    {
                        let admissions = preview.operations.get(&intent.operation_id)
                            .and_then(|record| record.ordinary.as_ref())
                            .map(|ordinary| ordinary.ready_admissions.clone())
                            .unwrap_or_default();
                        for candidate in preview.operations.values_mut() {
                            if candidate.intent.goal().and_then(|goal| goal.remote())
                                .is_some_and(|remote| admissions.contains(remote))
                                && let Some(ordinary) = candidate.ordinary.as_mut() {
                                ordinary.logically_retired = true;
                            }
                        }
                    }
                }
                _ => {
                    return Err(crate::host::HostError::DurableEffectConflict(
                        "public instruction is already started or not in flight".into(),
                    ));
                }
            }
            write_agent_state(transaction, &state)
        })
    }
}

fn retire_history_barriers(preview: &mut PublicV1OperationState, intent: &PublicOperationIntent) {
    use crate::protocol::public_operations::PublicOperationClass;
    if matches!(
        intent.class,
        PublicOperationClass::Close
            | PublicOperationClass::Abort
            | PublicOperationClass::TransientFault
            | PublicOperationClass::PermanentFault
    ) {
        for barrier in preview.history_barriers.values_mut() {
            barrier
                .retired_by
                .get_or_insert_with(|| intent.operation_id.clone());
        }
    }
}

fn valid_public_operation_transition(
    expected: PublicOperationStage,
    next: PublicOperationStage,
) -> bool {
    matches!(
        (expected, next),
        (
            PublicOperationStage::WaitingForContainment,
            PublicOperationStage::Ready
        ) | (
            PublicOperationStage::WaitingForContainment,
            PublicOperationStage::ContainmentPending
        ) | (
            PublicOperationStage::WaitingForContainment,
            PublicOperationStage::Completed
        ) | (PublicOperationStage::Ready, PublicOperationStage::Running)
            | (PublicOperationStage::Running, PublicOperationStage::Ready)
            | (
                PublicOperationStage::Ready,
                PublicOperationStage::ContainmentPending
            )
            | (PublicOperationStage::Ready, PublicOperationStage::Completed)
            | (
                PublicOperationStage::Running,
                PublicOperationStage::CallbackApplied
            )
            | (
                PublicOperationStage::CallbackApplied,
                PublicOperationStage::Completed
            )
            | (
                PublicOperationStage::CallbackApplied,
                PublicOperationStage::ContainmentPending
            )
            | (
                PublicOperationStage::Running,
                PublicOperationStage::ContainmentPending
            )
            | (
                PublicOperationStage::Running,
                PublicOperationStage::Completed
            )
            | (
                PublicOperationStage::ContainmentPending,
                PublicOperationStage::Completed
            )
            | (
                PublicOperationStage::RetryPending,
                PublicOperationStage::Completed
            )
            | (
                PublicOperationStage::RetryPending,
                PublicOperationStage::ContainmentPending
            )
    )
}

fn validate_expected_identity(
    actual: &StorageIdentity,
    expected: Option<&StorageIdentity>,
) -> Result<()> {
    if let Some(expected) = expected {
        let mut expected = expected.clone();
        expected.schema_version = PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION;
        if actual != &expected {
            return Err(crate::host::HostError::IdentityMismatch(
                "resource, Pod, PVC, replica incarnation, generation, or initialization changed"
                    .into(),
            ));
        }
    }
    Ok(())
}

fn classify_replacement_format(path: &Path) -> Result<u32> {
    if !path.is_file() {
        return Err(crate::host::HostError::MissingEstablishedStore);
    }
    let version = read_schema_version_immutable(path).map_err(|error| {
        crate::host::HostError::Corrupt(format!(
            "cannot classify existing SQLite metadata: {error}"
        ))
    })?;
    if version != PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION {
        return Err(crate::host::HostError::SchemaMismatch {
            expected: PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION,
            observed: version,
        });
    }
    Ok(version)
}

fn classify_replacement_store(
    path: &Path,
    expected_protocol: &PublicV1Identity,
    expected_binding: Option<&PublicV1LifecycleBinding>,
    validate_identity: impl Fn(&StorageIdentity) -> Result<()>,
) -> Result<AgentState> {
    let version = classify_replacement_format(path)?;
    recover_current_rollback_journal(
        path,
        expected_protocol,
        expected_binding,
        &validate_identity,
    )?;
    let connection = open_read_only_connection(path).map_err(|error| {
        crate::host::HostError::Corrupt(format!(
            "cannot validate replacement SQLite metadata: {error}"
        ))
    })?;
    let state = load_state_from_connection(&connection)?;
    if state.identity.schema_version != version {
        return Err(crate::host::HostError::SchemaMismatch {
            expected: version,
            observed: state.identity.schema_version,
        });
    }

    validate_identity(&state.identity)?;
    let replacement = state.public_v1_operation_state.as_ref().ok_or_else(|| {
        crate::host::HostError::InitializationNotAuthorized(
            "replacement store lacks Public V1 state".into(),
        )
    })?;
    if &replacement.identity != expected_protocol {
        return Err(crate::host::HostError::IdentityMismatch(
            "Public V1 protocol identity changed".into(),
        ));
    }
    if let Some(expected_binding) = expected_binding
        && replacement.binding.as_ref() != Some(expected_binding)
    {
        return Err(crate::host::HostError::IdentityMismatch(
            "Public V1 lifecycle binding changed".into(),
        ));
    }
    validate_public_v1_state(&state)?;
    validate_integrity(&connection)
        .map_err(|error| crate::host::HostError::Corrupt(error.to_string()))?;
    Ok(state)
}

fn recover_current_rollback_journal(
    path: &Path,
    expected_protocol: &PublicV1Identity,
    expected_binding: Option<&PublicV1LifecycleBinding>,
    validate_identity: &impl Fn(&StorageIdentity) -> Result<()>,
) -> Result<()> {
    let journal = PathBuf::from(format!("{}-journal", path.display()));
    if !journal.is_file() {
        return Ok(());
    }
    let parent = path.parent().ok_or_else(|| {
        crate::host::HostError::Corrupt("SQLite metadata path has no parent".into())
    })?;
    let recovery_root = parent.join(format!(
        ".kuberic-recovery-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| crate::host::HostError::Corrupt(error.to_string()))?
            .as_nanos()
    ));
    std::fs::create_dir(&recovery_root)?;
    let recovery_path = recovery_root.join("agent.sqlite3");
    let recovery_journal = PathBuf::from(format!("{}-journal", recovery_path.display()));
    let validation = (|| -> Result<()> {
        std::fs::copy(path, &recovery_path)?;
        std::fs::copy(&journal, &recovery_journal)?;
        let recovery = open_writable_connection(&recovery_path)?;
        let version: u32 = recovery.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version != PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION {
            return Err(crate::host::HostError::SchemaMismatch {
                expected: PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION,
                observed: version,
            });
        }
        drop(recovery);
        let recovery = open_read_only_connection(&recovery_path)?;
        let state = load_state_from_connection(&recovery)?;
        validate_identity(&state.identity)?;
        let replacement = state.public_v1_operation_state.as_ref().ok_or_else(|| {
            crate::host::HostError::InitializationNotAuthorized(
                "replacement store lacks Public V1 state".into(),
            )
        })?;
        if &replacement.identity != expected_protocol
            || expected_binding.is_some_and(|binding| replacement.binding.as_ref() != Some(binding))
        {
            return Err(crate::host::HostError::IdentityMismatch(
                "recovered Public V1 authority changed".into(),
            ));
        }
        validate_public_v1_state(&state)?;
        validate_integrity(&recovery)?;
        Ok(())
    })();
    let _ = std::fs::remove_dir_all(&recovery_root);
    validation?;
    let connection = open_writable_connection(path)?;
    let version: u32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION {
        return Err(crate::host::HostError::SchemaMismatch {
            expected: PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION,
            observed: version,
        });
    }
    drop(connection);
    Ok(())
}

fn read_schema_version_immutable(path: &Path) -> Result<u32> {
    let connection = open_read_only_connection_with_immutable(path, true)?;
    Ok(connection.query_row("PRAGMA user_version", [], |row| row.get(0))?)
}

fn open_read_only_connection(path: &Path) -> Result<Connection> {
    open_read_only_connection_with_immutable(path, false)
}

fn open_read_only_connection_with_immutable(path: &Path, immutable: bool) -> Result<Connection> {
    let absolute = std::path::absolute(path)?;
    let mut encoded = String::new();
    for byte in absolute.to_string_lossy().bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(&mut encoded, "%{byte:02X}").expect("writing to String cannot fail");
        }
    }
    let uri = if immutable {
        format!("file:{encoded}?mode=ro&immutable=1")
    } else {
        format!("file:{encoded}?mode=ro")
    };
    Ok(Connection::open_with_flags(
        uri,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

fn open_writable_connection(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

fn configure_durability(connection: &Connection) -> Result<()> {
    let journal_mode: String =
        connection.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("delete") {
        return Err(crate::host::HostError::Corrupt(format!(
            "SQLite refused rollback journaling and selected {journal_mode}"
        )));
    }
    connection.execute_batch(
        "PRAGMA synchronous = FULL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )?;
    let synchronous: i64 = connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?;
    if synchronous != 2 {
        return Err(crate::host::HostError::Corrupt(
            "SQLite synchronous mode is not FULL".into(),
        ));
    }
    Ok(())
}

fn create_schema(connection: &mut Connection, state: &AgentState) -> Result<()> {
    let transaction = connection.transaction()?;
    transaction.execute_batch(&format!(
        "CREATE TABLE {} (
            singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
            state_json TEXT NOT NULL
         );",
        PUBLIC_V1_AGENT_TABLES[0]
    ))?;
    transaction.pragma_update(None, "user_version", state.identity.schema_version)?;
    write_agent_state(&transaction, state)?;
    transaction.commit()?;
    Ok(())
}

fn validate_integrity(connection: &Connection) -> Result<()> {
    let result: String = connection.query_row("PRAGMA quick_check(1)", [], |row| row.get(0))?;
    if result != "ok" {
        return Err(crate::host::HostError::Corrupt(result));
    }
    Ok(())
}

fn load_state_from_connection(connection: &Connection) -> Result<AgentState> {
    let json: String = connection.query_row(
        "SELECT state_json FROM agent_state WHERE singleton = 1",
        [],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_str(&json)?)
}

fn write_agent_state(transaction: &Transaction<'_>, state: &AgentState) -> Result<()> {
    let json = serde_json::to_string(state)?;
    transaction.execute(
        "INSERT INTO agent_state(singleton, state_json) VALUES(1, ?1)
         ON CONFLICT(singleton) DO UPDATE SET state_json = excluded.state_json",
        [json],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::types::{
        AgentGeneration, EffectivePolicy, InitializationId, PodUid, PvcUid, ReplicaId,
        ReplicaIdentity, ReplicaInstanceId, ResourceUid,
    };

    fn identity() -> StorageIdentity {
        StorageIdentity {
            schema_version: crate::host::state::SCHEMA_VERSION,
            resource_uid: ResourceUid::new("scale-up-set"),
            pod_uid: PodUid::new("scale-up-pod"),
            pvc_uid: PvcUid::new("scale-up-pvc"),
            initialization_id: InitializationId::new("scale-up-init"),
            local_identity: ReplicaIdentity {
                replica_id: ReplicaId::new(2),
                instance_id: ReplicaInstanceId::new("scale-up-pod"),
                agent_generation: AgentGeneration::new("scale-up-generation"),
            },
            effective_policy: EffectivePolicy::fixed(2, 30).unwrap(),
        }
    }

    #[test]
    fn m3_retained_process_binding_requires_real_locked_open_not_metadata_or_uuid() {
        use crate::protocol::public_operations::StatePersistence;
        use crate::protocol::types::ProcessSessionId;
        let root = crate::host::tests::tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(root.path());
        let binding = PublicV1LifecycleBinding {
            identity: PublicV1Identity::new(1),
            resource_uid: identity().resource_uid,
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let metadata = SqliteStore::create_replacement_authorized(
            &path,
            AgentState::new(identity()),
            binding.clone(),
        )
        .unwrap();
        let process = ProcessSessionId::new("process-owner");
        assert!(metadata.bind_process_owner(&process).is_err());
        assert!(!metadata.owns_process(&process));
        drop(metadata);
        let observed = crate::host::provisioning::ObservedStorageIdentity {
            resource_uid: identity().resource_uid,
            pod_uid: identity().pod_uid,
            pvc_uid: identity().pvc_uid,
            instance_id: identity().local_identity.instance_id,
        };
        let owner = SqliteStore::open_replacement_for_process(
            &path,
            &observed,
            identity().local_identity.replica_id,
            None,
            &binding,
        )
        .unwrap();
        owner.bind_process_owner(&process).unwrap();
        assert!(owner.owns_process(&process));
        assert!(
            owner
                .bind_process_owner(&ProcessSessionId::new("new-uuid"))
                .is_err()
        );
        assert!(!owner.owns_process(&ProcessSessionId::new("new-uuid")));
        assert!(
            SqliteStore::open_replacement_for_process(
                &path,
                &observed,
                identity().local_identity.replica_id,
                None,
                &binding,
            )
            .is_err()
        );
        owner.release_process_storage();
        assert!(!owner.owns_process(&process));
        assert!(owner.bind_process_owner(&process).is_err());
    }

    #[test]
    fn m3_retained_final19_rejects18_readonly_cause_without_mutation() {
        use crate::protocol::public_operations::StatePersistence;
        assert_eq!(
            PUBLIC_V1_REPLACEMENT_SCHEMA_VERSION, 19,
            "requires coordinated matched format"
        );
        let root = crate::host::tests::tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(root.path());
        let binding = PublicV1LifecycleBinding {
            identity: PublicV1Identity::new(1),
            resource_uid: identity().resource_uid,
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let store =
            SqliteStore::create_replacement_authorized(&path, AgentState::new(identity()), binding)
                .unwrap();
        drop(store);
        let connection = Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 18).unwrap();
        drop(connection);
        let before = std::fs::read(&path).unwrap();
        assert!(SqliteStore::read_retained_recovery_state(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(!path.with_extension("owner").exists());
    }

    #[tokio::test]
    async fn ordinary_store_reopen_retains_exact_history_and_storage() {
        use crate::protocol::public_operations::*;
        use crate::protocol::types::*;
        let directory = crate::host::tests::tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let binding = PublicV1LifecycleBinding {
            identity: PublicV1Identity::new(1),
            resource_uid: identity().resource_uid,
            spec_generation: 1,
            state_persistence: StatePersistence::Persisted,
        };
        let store = SqliteStore::create_replacement_authorized(
            &path,
            AgentState::new(identity()),
            binding.clone(),
        )
        .unwrap();
        let initialization = derive_history_initialization_id(
            &binding.resource_uid,
            PublicProtocolGeneration::new(1),
        );
        let history = derive_bootstrap_history(
            &binding.resource_uid,
            PublicProtocolGeneration::new(1),
            &initialization,
        );
        let target = ReplicaStorageIncarnationContext {
            replica: identity().local_identity,
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: identity().pod_uid,
                pvc_uid: identity().pvc_uid,
            },
            storage_generation: StorageGeneration::new("target"),
        };
        store
            .initialize_public_v1_history(&initialization, &history, &target)
            .await
            .unwrap();
        let before = store.load_state().await.unwrap();
        drop(store);
        let reopened =
            SqliteStore::open_replacement_bound_existing(&path, Some(&identity()), &binding)
                .unwrap();
        assert_eq!(reopened.load_state().await.unwrap(), before);
        let preview = before.public_v1_operation_state.unwrap();
        assert_eq!(preview.history, Some(history));
        assert_eq!(preview.storage_incarnation, Some(target));
    }

    #[tokio::test]
    async fn m3_predecessor_formats_16_17_and_18_reject_before_retained_data_or_metadata_mutation()
    {
        use crate::protocol::public_operations::*;
        for predecessor in [16, 17, 18] {
            let directory = crate::host::tests::tempdir().unwrap();
            let path = SqliteStore::metadata_database_path(directory.path());
            let application = directory.path().join("acknowledged-application-data");
            std::fs::write(&application, b"acknowledged-prefix-and-history").unwrap();
            let binding = PublicV1LifecycleBinding {
                identity: PublicV1Identity::new(1),
                resource_uid: identity().resource_uid,
                spec_generation: 1,
                state_persistence: StatePersistence::Persisted,
            };
            let store = SqliteStore::create_replacement_authorized(
                &path,
                AgentState::new(identity()),
                binding.clone(),
            )
            .unwrap();
            let mut state = store.load_state().await.unwrap();
            drop(store);
            state.identity.schema_version = predecessor;
            state
                .public_v1_operation_state
                .as_mut()
                .unwrap()
                .identity
                .protocol_version = predecessor;
            let connection = Connection::open(&path).unwrap();
            connection
                .execute(
                    "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
                    [serde_json::to_string(&state).unwrap()],
                )
                .unwrap();
            connection
                .pragma_update(None, "user_version", predecessor)
                .unwrap();
            drop(connection);
            let before_database = std::fs::read(&path).unwrap();
            let before_application = std::fs::read(&application).unwrap();
            let before_names = std::fs::read_dir(directory.path())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .collect::<std::collections::BTreeSet<_>>();
            for _ in 0..2 {
                assert!(
                    SqliteStore::open_replacement_bound_existing(
                        &path,
                        Some(&identity()),
                        &binding
                    )
                    .is_err()
                );
                let observed = crate::host::provisioning::ObservedStorageIdentity {
                    resource_uid: identity().resource_uid,
                    pod_uid: identity().pod_uid,
                    pvc_uid: identity().pvc_uid,
                    instance_id: identity().local_identity.instance_id,
                };
                assert!(
                    SqliteStore::open_replacement_for_process(
                        &path,
                        &observed,
                        identity().local_identity.replica_id,
                        None,
                        &binding,
                    )
                    .is_err()
                );
                assert!(!path.with_extension("owner").exists());
                assert_eq!(std::fs::read(&path).unwrap(), before_database);
                assert_eq!(std::fs::read(&application).unwrap(), before_application);
                assert_eq!(
                    std::fs::read_dir(directory.path())
                        .unwrap()
                        .map(|entry| entry.unwrap().file_name())
                        .collect::<std::collections::BTreeSet<_>>(),
                    before_names
                );
            }
        }
    }

    #[test]
    fn m3_storage_lifetime_excludes_overlap_and_crash_releases_os_ownership() {
        const CHILD: &str = "KUBERIC_M3_STORAGE_OWNER_CHILD";
        if let Some(root) = std::env::var_os(CHILD) {
            let root = PathBuf::from(root);
            let files =
                SqliteStore::lock_process_storage(&root.join("agent.sqlite3"), None).unwrap();
            let binding = PublicV1LifecycleBinding {
                identity: PublicV1Identity::new(1),
                resource_uid: identity().resource_uid,
                spec_generation: 1,
                state_persistence: crate::protocol::public_operations::StatePersistence::Persisted,
            };
            let mut store = SqliteStore::create_replacement_authorized(
                root.join("agent.sqlite3"),
                AgentState::new(identity()),
                binding,
            )
            .unwrap();
            *store.process_storage.get_mut().unwrap() = Some(files);
            // Model a lost cleanup owner, not successful Close or task join.
            drop(store);
            std::fs::write(root.join("ready"), b"owned").unwrap();
            loop {
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../.paw/work/sf-hosting-m3")
            .join(format!("phase2-source-lock-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "host::sqlite_store::tests::m3_storage_lifetime_excludes_overlap_and_crash_releases_os_ownership",
                "--nocapture",
            ])
            .env(CHILD, &root)
            .spawn()
            .unwrap();
        let start = std::time::Instant::now();
        while !root.join("ready").is_file() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "child stopped before owning storage"
            );
            if start.elapsed() > std::time::Duration::from_secs(5) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("storage owner child did not become ready");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(SqliteStore::lock_process_storage(&root.join("agent.sqlite3"), None).is_err());
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        let ownership =
            SqliteStore::lock_process_storage(&root.join("agent.sqlite3"), None).unwrap();
        assert!(SqliteStore::lock_process_storage(&root.join("agent.sqlite3"), None).is_err());
        drop(ownership);
        assert!(SqliteStore::lock_process_storage(&root.join("agent.sqlite3"), None).is_ok());
        let application = BTreeMap::from([("application".into(), root.join("shared-app"))]);
        let first =
            SqliteStore::lock_process_storage(&root.join("first.sqlite3"), Some(&application))
                .unwrap();
        assert!(
            SqliteStore::lock_process_storage(&root.join("second.sqlite3"), Some(&application),)
                .is_err(),
            "distinct metadata paths must not confer shared application writer ownership"
        );
        drop(first);
        assert!(
            SqliteStore::lock_process_storage(&root.join("second.sqlite3"), Some(&application),)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn removed_ingress_fields_reject_before_metadata_mutation() {
        let directory = crate::host::tests::tempdir().unwrap();
        let path = SqliteStore::metadata_database_path(directory.path());
        let preview = crate::protocol::public_operations::PublicV1Identity::new(11);
        let binding = crate::protocol::public_operations::PublicV1LifecycleBinding {
            identity: preview.clone(),
            resource_uid: identity().resource_uid,
            spec_generation: 1,
            state_persistence: crate::protocol::public_operations::StatePersistence::Persisted,
        };
        let store = SqliteStore::create_replacement_authorized(
            &path,
            AgentState::new(identity()),
            binding.clone(),
        )
        .unwrap();
        let initialization = crate::protocol::types::derive_history_initialization_id(
            &binding.resource_uid,
            crate::protocol::types::PublicProtocolGeneration::new(11),
        );
        let history = crate::protocol::types::derive_bootstrap_history(
            &binding.resource_uid,
            crate::protocol::types::PublicProtocolGeneration::new(11),
            &initialization,
        );
        let target_storage = crate::protocol::types::ReplicaStorageIncarnationContext {
            replica: identity().local_identity,
            storage: crate::protocol::types::ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new("scale-up-pod"),
                pvc_uid: PvcUid::new("scale-up-pvc"),
            },
            storage_generation: crate::protocol::types::StorageGeneration::new(
                "target-storage-generation",
            ),
        };
        store
            .initialize_public_v1_history(&initialization, &history, &target_storage)
            .await
            .unwrap();
        let valid = serde_json::to_value(store.load_state().await.unwrap()).unwrap();
        drop(store);
        for (field, value) in [
            ("currentInboundBuildAuthority", serde_json::Value::Null),
            ("inboundBuildGeneration", serde_json::json!(7)),
            ("terminalInboundBuildReceipts", serde_json::json!({})),
        ] {
            let mut mixed = valid.clone();
            mixed["publicV1OperationState"][field] = value;
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute(
                    "UPDATE agent_state SET state_json = ?1 WHERE singleton = 1",
                    [serde_json::to_string(&mixed).unwrap()],
                )
                .unwrap();
            drop(connection);
            let before = std::fs::read(&path).unwrap();
            assert!(
                SqliteStore::open_replacement_bound_existing(&path, Some(&identity()), &binding,)
                    .is_err()
            );
            let observed = crate::host::provisioning::ObservedStorageIdentity {
                resource_uid: identity().resource_uid,
                pod_uid: identity().pod_uid,
                pvc_uid: identity().pvc_uid,
                instance_id: identity().local_identity.instance_id,
            };
            assert!(
                SqliteStore::open_replacement_for_process(
                    &path,
                    &observed,
                    identity().local_identity.replica_id,
                    None,
                    &binding,
                )
                .is_err()
            );
            assert!(!path.with_extension("owner").exists());
            assert_eq!(std::fs::read(&path).unwrap(), before);
        }
    }
}
