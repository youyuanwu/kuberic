use crate::native::PgReplicationEvidence;
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

use async_trait::async_trait;
use kuberic_protocol::types::{
    AccessStatus, BuildAuthority, ConfigurationDescriptor, Epoch, FaultType, OperationId,
    ReplicaId, ReplicaIdentity, ReplicaRole,
};
use kuberic_runtime::replicator::{
    PrimaryReplicator, ReplicaInformation, ReplicaSetConfiguration, ReplicaSetQuorumMode,
    Replicator, StatefulServicePartition,
};
use kuberic_runtime::{Result, RuntimeError};
use tokio::sync::{Mutex, RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use crate::access::{PgAccessController, initialize_application_role, stop_fence};
use crate::build::{
    BUILD_PROTOCOL_VERSION, MAX_BUILDS, PgBuildMethod, PgBuildProgress, PgBuildRequest,
    PgBuildStage, PgLineage, decode, encode,
};
use crate::durable::{PgDurableRole, PgDurableState, PgDurableStore, PgRecoveryState};
use crate::instance::{PgError, PgInstanceManager};
use crate::native::{PgNativeObserver, compile_synchronous_configuration, timeline_history_digest};

pub(crate) fn application_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::Application(error.to_string())
}

fn evidence_configuration(
    snapshot: &crate::native::PgObservation,
) -> Option<&kuberic_protocol::types::ConfigurationId> {
    snapshot
        .evidence
        .as_ref()?
        .synchronous
        .as_ref()
        .filter(|s| s.valid)
        .map(|s| &s.configuration_id)
}

pub(crate) async fn report_failure(
    partition: &StatefulServicePartition,
    fault: FaultType,
    error: impl std::fmt::Display,
) -> RuntimeError {
    let error = error.to_string();
    match tokio::time::timeout(
        std::time::Duration::from_secs(5),
        partition.report_fault(fault),
    )
    .await
    {
        Ok(Ok(())) => application_error(error),
        Ok(Err(report)) => application_error(format!("{error}; fault acknowledgement: {report}")),
        Err(_) => application_error(format!("{error}; fault acknowledgement timed out")),
    }
}

pub(crate) fn unsupported<T>(operation: &str) -> Result<T> {
    Err(application_error(format!(
        "PostgreSQL v2: {operation} is unsupported at this migration stage"
    )))
}

struct DriverState {
    authority: Option<PgConfiguration>,
    role: ReplicaRole,
    opened: bool,
}

struct PgConfiguration {
    local_identity: ReplicaIdentity,
    current_configuration: ConfigurationDescriptor,
}

pub struct PgReplicator {
    instance: Arc<PgInstanceManager>,
    durable: Arc<PgDurableStore>,
    observer: PgNativeObserver,
    fault_tx: mpsc::Sender<FaultType>,
    partition: Weak<StatefulServicePartition>,
    cancellation: CancellationToken,
    coordination: crate::data_service::PgCoordination,
    initializing: bool,
    state: Mutex<DriverState>,
    configuration: RwLock<Option<ReplicaSetConfiguration>>,
    build_cancellation: std::sync::Mutex<CancellationToken>,
    #[cfg(feature = "testing")]
    build_gate: std::sync::Mutex<Option<Arc<crate::build::BuildGate>>>,
}

impl PgReplicator {
    async fn report(&self, fault: FaultType, error: impl std::fmt::Display) -> RuntimeError {
        match self.partition.upgrade() {
            Some(partition) => report_failure(&partition, fault, error).await,
            None => application_error(format!(
                "{error}; fault acknowledgement: {}",
                RuntimeError::Closed
            )),
        }
    }

    async fn permanent(&self, error: impl std::fmt::Display) -> RuntimeError {
        let error = PgError::Process(error.to_string()).with_cleanup(self.instance.abort_owned());
        self.report(FaultType::Permanent, error).await
    }

    async fn pg_result<T>(&self, result: std::result::Result<T, PgError>) -> Result<T> {
        let error = match result {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let fault = error.fault_type();
        let error = error.with_cleanup(self.instance.abort_owned());
        Err(self.report(fault, error).await)
    }

    async fn permanent_result<T>(
        &self,
        result: std::result::Result<T, impl std::fmt::Display>,
    ) -> Result<T> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => Err(self.permanent(error).await),
        }
    }

    pub(crate) async fn new(
        instance: Arc<PgInstanceManager>,
        durable: Arc<PgDurableStore>,
        fault_tx: mpsc::Sender<FaultType>,
        partition: Weak<StatefulServicePartition>,
        cancellation: CancellationToken,
        coordination: crate::data_service::PgCoordination,
        initializing: bool,
    ) -> Self {
        Self {
            observer: PgNativeObserver::with_store(instance.clone(), durable.clone()).await,
            instance,
            durable,
            fault_tx,
            partition,
            cancellation,
            coordination,
            initializing,
            configuration: RwLock::new(None),
            build_cancellation: std::sync::Mutex::new(CancellationToken::new()),
            #[cfg(feature = "testing")]
            build_gate: std::sync::Mutex::new(None),
            state: Mutex::new(DriverState {
                authority: None,
                role: ReplicaRole::None,
                opened: false,
            }),
        }
    }

    async fn validate(&self) -> Result<PgDurableState> {
        let durable = self
            .permanent_result(self.durable.revalidate().await)
            .await?;
        if durable.recovery_state == PgRecoveryState::Unsafe {
            return Err(self.permanent("unsafe PostgreSQL durable state").await);
        }
        if durable.native_build.is_some() {
            return Ok(durable);
        }
        if durable.recovery_state != PgRecoveryState::Ready || durable.accepted_build.is_some() {
            return Err(application_error("incomplete PostgreSQL recovery metadata"));
        }
        if let Some(system) = &durable.system_identifier {
            if durable.role == PgDurableRole::None
                || (durable.role == PgDurableRole::Standby)
                    != self.instance.data_dir().join("standby.signal").exists()
                || self.instance.data_dir().join("recovery.signal").exists()
            {
                return Err(application_error(
                    "PostgreSQL durable recovery role differs from data",
                ));
            }
            let (actual_system, timeline) = self
                .permanent_result(self.instance.control_identity().await)
                .await?;
            if system != &actual_system || durable.timeline_id != Some(timeline) {
                return Err(self
                    .permanent("PostgreSQL system identity/timeline mismatch")
                    .await);
            }
            let evidence = PgReplicationEvidence {
                engine: "postgres-physical".into(),
                system_identifier: actual_system,
                timeline_id: timeline,
                in_recovery: durable.role == PgDurableRole::Standby,
                flush_lsn: durable.flush_lsn,
                received_lsn: durable.received_lsn,
                replay_lsn: durable.replay_lsn,
                metadata_generation: durable.generation,
                synchronous: durable.synchronous.clone(),
                wal_receiver_stopped: false,
            };
            let digest = self
                .permanent_result(
                    timeline_history_digest(self.instance.data_dir(), &evidence).await,
                )
                .await?;
            if durable.timeline_history_digest.as_ref() != Some(&digest) {
                return Err(self.permanent("PostgreSQL timeline history mismatch").await);
            }
        } else {
            if !self.initializing {
                return Err(self
                    .permanent("established PostgreSQL lacks durable lineage")
                    .await);
            }
            if !self
                .permanent_result(crate::service::is_empty(self.instance.data_dir()))
                .await?
            {
                if self.instance.data_dir().join("standby.signal").exists()
                    || self.instance.data_dir().join("recovery.signal").exists()
                {
                    return Err(self
                        .permanent("initial PostgreSQL storage is in recovery")
                        .await);
                }
                // An authorized retry may reuse completed initdb, never erase partial PGDATA.
                self.permanent_result(self.instance.control_identity().await)
                    .await?;
            }
        }
        Ok(durable)
    }

    async fn current_sessions(
        &self,
        configuration: &ConfigurationDescriptor,
    ) -> Result<BTreeMap<ReplicaIdentity, kuberic_protocol::types::ProcessSessionId>> {
        let local = self.durable.snapshot().await.identity.replica;
        let mut sessions = BTreeMap::new();
        for member in &configuration.members {
            if member.identity != local {
                sessions.insert(
                    member.identity.clone(),
                    self.peer_session(&member.identity).await?,
                );
            }
        }
        Ok(sessions)
    }

    async fn observe_pg(&self) -> Result<crate::native::PgObservation> {
        let durable = self.validate().await?;
        let mut snapshot = self.pg_result(self.observer.snapshot().await).await?;
        let evidence = snapshot
            .evidence
            .as_ref()
            .ok_or_else(|| application_error("missing native evidence"))?;
        if let Some(build) = &durable.native_build
            && (evidence.system_identifier != build.request.lineage.system_identifier
                || evidence.timeline_id != build.request.lineage.timeline
                || !evidence.in_recovery
                || PgLineage::read(
                    self.instance.data_dir(),
                    evidence.system_identifier.clone(),
                    evidence.timeline_id,
                )
                .await
                .map_err(application_error)?
                    != build.request.lineage)
        {
            return Err(self
                .permanent("native PostgreSQL build storage no longer matches frozen lineage")
                .await);
        }
        if durable.native_build.is_none()
            && (durable
                .system_identifier
                .as_ref()
                .is_some_and(|system| system != &evidence.system_identifier)
                || durable
                    .timeline_id
                    .is_some_and(|timeline| timeline != evidence.timeline_id)
                || snapshot.current_lsn < durable.current_lsn
                || evidence.flush_lsn < durable.flush_lsn)
        {
            return Err(self
                .permanent("PostgreSQL live evidence differs from durable lineage/progress")
                .await);
        }
        let digest = self
            .permanent_result(timeline_history_digest(self.instance.data_dir(), evidence).await)
            .await?;
        let mut next = durable.clone();
        next.system_identifier = Some(evidence.system_identifier.clone());
        next.timeline_id = Some(evidence.timeline_id);
        next.timeline_history_digest = Some(digest);
        next.role = if evidence.in_recovery {
            PgDurableRole::Standby
        } else {
            PgDurableRole::Primary
        };
        next.current_lsn = snapshot.current_lsn;
        next.flush_lsn = evidence.flush_lsn;
        next.received_lsn = evidence.received_lsn;
        next.replay_lsn = evidence.replay_lsn;
        // Invalidation removes current quorum credit, not historical acknowledged durability.
        next.policy_certified_lsn = next.policy_certified_lsn.max(snapshot.committed_lsn);
        let updated = if next == durable {
            durable
        } else {
            self.permanent_result(
                self.durable
                    .update(|state| {
                        *state = next;
                        Ok(())
                    })
                    .await,
            )
            .await?
        };
        snapshot
            .evidence
            .as_mut()
            .expect("checked evidence")
            .metadata_generation = updated.generation;
        if let Some((configuration, boundary)) = &updated.catch_up
            && evidence_configuration(&snapshot) == Some(configuration)
        {
            snapshot.catch_up_boundary = Some(*boundary);
            snapshot.catch_up_complete = snapshot.committed_lsn >= *boundary;
        }
        Ok(snapshot)
    }

    async fn peer_session(
        &self,
        identity: &ReplicaIdentity,
    ) -> Result<kuberic_protocol::types::ProcessSessionId> {
        self.configuration
            .read()
            .await
            .as_ref()
            .and_then(|c| {
                c.replicas
                    .iter()
                    .find(|r| &r.identity == identity && !r.process_session_id.is_empty())
            })
            .map(|r| r.process_session_id.clone())
            .ok_or(RuntimeError::AuthorityNotAdmitted)
    }

    async fn validate_build(&self, request: &PgBuildRequest) -> Result<()> {
        request.validate().map_err(application_error)?;
        if self.cancellation.is_cancelled() {
            return Err(RuntimeError::OperationCancelled);
        }
        let durable = self.durable.snapshot().await;
        let configuration = self.configuration.read().await;
        let configuration = configuration
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let target = configuration
            .replicas
            .iter()
            .find(|r| {
                r.identity == request.authority.target && r.build_id == request.authority.build_id
            })
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let source = configuration
            .replicas
            .iter()
            .find(|r| r.identity == request.authority.source && r.role == ReplicaRole::Primary)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if durable.identity.resource_uid != request.resource_uid
            || configuration.configuration != request.authority.current_configuration
            || source.process_session_id != request.source_session
            || target.process_session_id != request.target_session
            || target.current_progress != request.authority.replication_boundary_lsn
            || target.role != ReplicaRole::IdleSecondary
            || ![&request.authority.source, &request.authority.target]
                .contains(&&durable.identity.replica)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    async fn lineage(&self) -> Result<PgLineage> {
        let (system, timeline) = if self.instance.is_running().await {
            let evidence = self
                .observer
                .snapshot()
                .await
                .map_err(application_error)?
                .evidence
                .ok_or_else(|| application_error("missing PostgreSQL lineage"))?;
            (evidence.system_identifier, evidence.timeline_id)
        } else {
            self.instance
                .control_identity()
                .await
                .map_err(application_error)?
        };
        PgLineage::read(self.instance.data_dir(), system, timeline)
            .await
            .map_err(application_error)
    }

    fn rpc_request(
        &self,
        request: &PgBuildRequest,
    ) -> Result<tonic::Request<crate::proto::NativeBuildRequest>> {
        if self.coordination.bearer_token.is_empty() {
            return Err(application_error(
                "native coordination credentials are not configured",
            ));
        }
        let mut message = tonic::Request::new(crate::proto::NativeBuildRequest {
            envelope_json: encode(request).map_err(application_error)?,
        });
        message.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", self.coordination.bearer_token)
                .parse()
                .map_err(application_error)?,
        );
        Ok(message)
    }

    pub async fn inspect_source(&self, request: &PgBuildRequest) -> Result<PgLineage> {
        let _state = self.state.lock().await;
        self.validate_build(request).await?;
        let durable = self.durable.revalidate().await.map_err(application_error)?;
        if durable.identity.replica != request.authority.source
            || !durable
                .outbound_builds
                .iter()
                .any(|build| &build.request == request)
            || request.source_endpoint != self.coordination.local_endpoint
            || request.source_host != self.instance.listen_host()
            || request.source_port != self.instance.port()
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let snapshot = self.observe_pg().await?;
        let lineage = self.lineage().await?;
        if lineage != request.lineage
            || snapshot.evidence.is_none_or(|e| e.in_recovery)
            || snapshot.committed_lsn < request.authority.replication_boundary_lsn
        {
            return Err(application_error(
                "source lineage/recovery boundary changed",
            ));
        }
        Ok(lineage)
    }

    async fn check_source(&self, request: &PgBuildRequest) -> Result<()> {
        self.validate_build(request).await?;
        let mut client = crate::proto::pg_data_service_client::PgDataServiceClient::connect(
            request.source_endpoint.clone(),
        )
        .await
        .map_err(application_error)?;
        let response = client
            .inspect_source(self.rpc_request(request)?)
            .await
            .map_err(application_error)?
            .into_inner();
        let lineage: PgLineage = decode(&response.lineage_json).map_err(application_error)?;
        if lineage != request.lineage {
            return Err(application_error("source lineage mismatch"));
        }
        Ok(())
    }

    async fn persist_build(&self, progress: &PgBuildProgress) -> Result<()> {
        self.validate_build(&progress.request).await?;
        self.durable
            .update(|state| {
                state.accepted_build = Some(progress.request.authority.clone());
                state.native_build = Some(progress.clone());
                state.external_access_closed = true;
                state.recovery_state = PgRecoveryState::Rebuilding;
                state.synchronous = None;
                Ok(())
            })
            .await
            .map_err(application_error)?;
        Ok(())
    }

    pub async fn receive_build(&self, request: PgBuildRequest) -> Result<PgBuildProgress> {
        let state = self.state.lock().await;
        self.validate_build(&request).await?;
        let metadata = self.durable.snapshot().await;
        let admitted_storage = state.authority.is_some()
            || metadata.has_accepted_authority
            || metadata.synchronous.is_some()
            || metadata.native_build.is_some();
        if state.role != ReplicaRole::IdleSecondary
            || !self
                .durable
                .revalidate()
                .await
                .map_err(application_error)?
                .external_access_closed
        {
            return Err(RuntimeError::ReconfigurationPending);
        }
        if state.authority.as_ref().is_some_and(|authority| {
            authority.current_configuration.epoch > request.authority.current_configuration.epoch
                || authority.current_configuration.epoch
                    == request.authority.current_configuration.epoch
                    && authority.current_configuration != request.authority.current_configuration
        }) {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let mut cleanup = BuildCleanup {
            instance: self.instance.clone(),
            armed: true,
        };
        let cancellation = self.build_cancellation.lock().unwrap().clone();
        let work = self.receive_build_inner(request, admitted_storage);
        let result = tokio::select! {
            biased;
            _ = self.cancellation.cancelled() => Err(RuntimeError::OperationCancelled),
            _ = cancellation.cancelled() => Err(RuntimeError::OperationCancelled),
            result = tokio::time::timeout(std::time::Duration::from_secs(60), work) =>
                result.unwrap_or_else(|_| Err(application_error("native build timed out"))),
        };
        if let Err(error) = &result
            && let Err(cleanup) = self.instance.stop().await
        {
            return Err(self
                .report(
                    FaultType::Permanent,
                    format!("{}; native build cleanup: {cleanup}", error),
                )
                .await);
        }
        cleanup.armed = false;
        result
    }

    async fn receive_build_inner(
        &self,
        request: PgBuildRequest,
        admitted_storage: bool,
    ) -> Result<PgBuildProgress> {
        self.check_source(&request).await?;
        let durable = self.durable.revalidate().await.map_err(application_error)?;
        if durable.identity.replica != request.authority.target {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        let mut progress = match durable.native_build {
            Some(mut previous) if previous.request.same_work(&request) => {
                // A fresh process session must produce fresh live completion evidence.
                if previous.stage == PgBuildStage::Complete {
                    previous.stage = PgBuildStage::Installed;
                    previous.evidence = None;
                }
                if previous.request.source_session != request.source_session
                    || previous.request.target_session != request.target_session
                {
                    previous.evidence = None;
                }
                previous.request = request.clone();
                previous
            }
            Some(previous) if previous.request.authority.build_id == request.authority.build_id => {
                return Err(application_error(
                    "native build ID reused with changed lineage",
                ));
            }
            _ => {
                let method = if durable.system_identifier.is_some() && admitted_storage {
                    let local = self.lineage().await?;
                    if !local.can_rewind_from(&request.lineage) {
                        return Err(application_error(
                            "incompatible PostgreSQL system identity/timeline",
                        ));
                    }
                    PgBuildMethod::Rewind
                } else {
                    PgBuildMethod::Fresh
                };
                PgBuildProgress {
                    request: request.clone(),
                    stage: PgBuildStage::Intent,
                    method,
                    sequence: 1,
                    evidence: None,
                }
            }
        };
        self.persist_build(&progress).await?;
        if progress.stage == PgBuildStage::Intent {
            self.build_checkpoint(PgBuildStage::Intent).await;
        }
        self.validate_build(&request).await?;
        self.pg_result(self.instance.prepare_native_build().await)
            .await?;
        if matches!(progress.stage, PgBuildStage::Intent | PgBuildStage::Copying) {
            // Interrupted rewind/backup is not reusable installed data.
            if progress.stage == PgBuildStage::Copying {
                progress.method = PgBuildMethod::Fresh;
            }
            progress
                .advance(PgBuildStage::Copying)
                .map_err(application_error)?;
            self.persist_build(&progress).await?;
            let rewound = if progress.method == PgBuildMethod::Rewind {
                self.check_source(&request).await?;
                self.pg_result(
                    self.instance
                        .rewind_for_build(&request.source_host, request.source_port)
                        .await,
                )
                .await?
            } else {
                false
            };
            if !rewound {
                progress.method = PgBuildMethod::Fresh;
                self.persist_build(&progress).await?;
                self.check_source(&request).await?;
                self.clear_pgdata().await?;
                self.validate_build(&request).await?;
                self.pg_result(
                    self.instance
                        .base_backup(&request.source_host, request.source_port)
                        .await,
                )
                .await?;
            }
            let installed = self.lineage().await?;
            if installed != request.lineage
                && !(rewound && installed.can_rewind_from(&request.lineage))
            {
                return Err(application_error(
                    "installed backup has incompatible PostgreSQL lineage",
                ));
            }
            progress
                .advance(PgBuildStage::Installed)
                .map_err(application_error)?;
            self.persist_build(&progress).await?;
            self.build_checkpoint(PgBuildStage::Installed).await;
        }
        self.check_source(&request).await?;
        self.pg_result(
            self.instance
                .config()
                .configure_standby(
                    self.instance.data_dir(),
                    &request.source_host,
                    request.source_port,
                    &crate::native::replication_application_name(
                        &request.authority.target,
                        &request.target_session,
                    ),
                    &request.lineage,
                )
                .await,
        )
        .await?;
        progress
            .advance(PgBuildStage::Recovering)
            .map_err(application_error)?;
        self.persist_build(&progress).await?;
        self.validate_build(&request).await?;
        self.pg_result(
            self.instance
                .start_native_with_cancellation(self.fault_tx.clone(), self.cancellation.clone())
                .await,
        )
        .await?;
        self.build_checkpoint(PgBuildStage::Recovering).await;
        loop {
            self.validate_build(&request).await?;
            let (control, _) = self.instance.connect().await.map_err(application_error)?;
            control
                .simple_query("CHECKPOINT")
                .await
                .map_err(application_error)?;
            drop(control);
            let snapshot = self.pg_result(self.observer.snapshot().await).await?;
            progress.evidence = snapshot.evidence;
            if let Some(evidence) = &progress.evidence {
                if evidence.system_identifier != request.lineage.system_identifier
                    || !evidence.in_recovery
                {
                    return Err(application_error(
                        "recovery changed PostgreSQL system identity or role",
                    ));
                }
                if evidence.timeline_id != request.lineage.timeline
                    && request
                        .lineage
                        .history
                        .iter()
                        .any(|fork| fork.timeline == evidence.timeline_id)
                    || evidence
                        .replay_lsn
                        .zip(evidence.received_lsn)
                        .is_some_and(|(replay, received)| replay > received)
                {
                    tracing::debug!(
                        ?evidence,
                        "waiting for installed WAL to join the frozen streaming timeline"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    continue;
                }
            }
            progress
                .validate()
                .map_err(|error| application_error(format!("{error}: {:?}", progress.evidence)))?;
            if progress.recovered() {
                self.check_source(&request).await?;
                if self.lineage().await? != request.lineage {
                    return Err(application_error(
                        "recovered timeline differs from frozen source",
                    ));
                }
                progress
                    .advance(PgBuildStage::Complete)
                    .map_err(application_error)?;
                self.persist_build(&progress).await?;
                self.observe_pg().await?;
                self.durable
                    .update(|state| {
                        state.recovery_state = PgRecoveryState::Ready;
                        state.postgres_stopped = false;
                        Ok(())
                    })
                    .await
                    .map_err(application_error)?;
                self.build_checkpoint(PgBuildStage::Complete).await;
                self.validate_build(&request).await?;
                return Ok(progress);
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    async fn build_checkpoint(&self, _stage: PgBuildStage) {
        #[cfg(feature = "testing")]
        {
            let gate = self.build_gate.lock().unwrap().clone();
            if let Some(gate) = gate
                && gate.stage == _stage
            {
                gate.entered.notify_one();
                gate.release.notified().await;
            }
        }
    }

    #[cfg(feature = "testing")]
    pub fn pause_build(&self, stage: PgBuildStage) -> Arc<crate::build::BuildGate> {
        let gate = Arc::new(crate::build::BuildGate {
            stage,
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *self.build_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    #[cfg(feature = "testing")]
    pub async fn durable_state(&self) -> PgDurableState {
        self.durable.snapshot().await
    }

    async fn clear_pgdata(&self) -> Result<()> {
        if self.instance.is_running().await {
            return Err(application_error("cannot clear running PostgreSQL data"));
        }
        tokio::fs::create_dir_all(self.instance.data_dir())
            .await
            .map_err(application_error)?;
        let mut entries = tokio::fs::read_dir(self.instance.data_dir())
            .await
            .map_err(application_error)?;
        while let Some(entry) = entries.next_entry().await.map_err(application_error)? {
            let kind = entry.file_type().await.map_err(application_error)?;
            if kind.is_dir() {
                tokio::fs::remove_dir_all(entry.path())
                    .await
                    .map_err(application_error)?;
            } else {
                tokio::fs::remove_file(entry.path())
                    .await
                    .map_err(application_error)?;
            }
        }

        tokio::fs::File::open(self.instance.data_dir())
            .await
            .map_err(application_error)?
            .sync_all()
            .await
            .map_err(application_error)
    }

    async fn close_access(&self) -> Result<()> {
        self.validate().await?;
        if self.instance.is_running().await {
            self.pg_result(
                PgAccessController::new(&self.instance)
                    .close_external()
                    .await,
            )
            .await?;
        }
        self.permanent_result(
            self.durable
                .update(|state| {
                    state.external_access_closed = true;
                    Ok(())
                })
                .await,
        )
        .await?;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.validate().await?;
        self.permanent_result(stop_fence(&self.instance).await)
            .await?;
        self.permanent_result(
            self.durable
                .update(|state| {
                    state.external_access_closed = true;
                    state.postgres_stopped = true;
                    Ok(())
                })
                .await,
        )
        .await?;
        Ok(())
    }
}

struct BuildCleanup {
    instance: Arc<PgInstanceManager>,
    armed: bool,
}

impl Drop for BuildCleanup {
    fn drop(&mut self) {
        if self.armed
            && let Err(error) = self.instance.abort_owned()
        {
            tracing::error!(%error, "native build cancellation cleanup failed");
        }
    }
}

impl PgReplicator {
    async fn open(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.opened || self.cancellation.is_cancelled() {
            return Err(RuntimeError::Closed);
        }
        let durable = self.validate().await?;
        if durable.native_build.is_some() {
            // No old session is allowed to reconnect a receiver on reopen.
            // Exact re-admission resumes installed data without initializing it.
            state.opened = true;
            return Ok(());
        }
        if self.initializing
            && durable.system_identifier.is_none()
            && self
                .permanent_result(crate::service::is_empty(self.instance.data_dir()))
                .await?
        {
            self.pg_result(self.instance.init_db().await).await?;
        }
        self.pg_result(
            self.instance
                .start_native_with_cancellation(self.fault_tx.clone(), self.cancellation.clone())
                .await,
        )
        .await?;
        let result = async {
            if self.initializing {
                self.pg_result(initialize_application_role(&self.instance).await)
                    .await?;
            }
            self.observe_pg().await?;
            self.permanent_result(
                self.durable
                    .update(|state| {
                        state.external_access_closed = true;
                        state.postgres_stopped = false;
                        Ok(())
                    })
                    .await,
            )
            .await?;
            Ok::<_, RuntimeError>(())
        }
        .await;
        if let Err(error) = result {
            return Err(match stop_fence(&self.instance).await {
                Ok(()) => error,
                Err(cleanup) => application_error(format!("{error}; open cleanup: {cleanup}")),
            });
        }
        state.opened = true;
        Ok(())
    }

    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        let mut state = self.state.lock().await;
        self.validate().await?;
        match role {
            ReplicaRole::Primary => {
                let authority = state
                    .authority
                    .as_ref()
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?;
                if authority.current_configuration.epoch != epoch {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                self.close_access().await?;
                if self
                    .pg_result(self.observer.snapshot().await)
                    .await?
                    .evidence
                    .is_none_or(|e| e.in_recovery)
                {
                    return unsupported("promotion");
                }
            }
            ReplicaRole::None => self.stop().await?,
            ReplicaRole::IdleSecondary => self.close_access().await?,
            ReplicaRole::ActiveSecondary => {
                let authority = state
                    .authority
                    .as_ref()
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?;
                if authority.current_configuration.epoch != epoch
                    || !authority.current_configuration.members.iter().any(|m| {
                        m.identity == authority.local_identity
                            && m.role == ReplicaRole::ActiveSecondary
                    })
                {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                self.close_access().await?;
            }
        }
        state.role = role;
        Ok(())
    }

    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        let state = self.state.lock().await;
        self.validate().await?;
        if state
            .authority
            .as_ref()
            .is_none_or(|a| a.current_configuration.epoch != epoch)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        Ok(())
    }

    pub(crate) async fn close(&self) -> Result<()> {
        let mut state = self.state.lock().await;
        if state.opened {
            if self.cancellation.is_cancelled() {
                // Abort sealed helper launches. Join its retained cleanup result
                // without starting control-data validation in a closed runtime.
                self.instance.stop().await.map_err(application_error)?;
            } else {
                self.stop().await?;
            }
            state.opened = false;
        }
        self.cancellation.cancel();
        Ok(())
    }

    fn abort(&self) {
        self.cancellation.cancel();
        if let Err(error) = self.instance.abort_owned() {
            tracing::error!(%error, "PostgreSQL driver abort cleanup failed");
        }
    }

    async fn observe(&self) -> Result<crate::native::PgObservation> {
        let _state = self.state.lock().await;
        if self.instance.is_running().await {
            self.observe_pg().await
        } else {
            let durable = self.validate().await?;
            Ok(crate::native::PgObservation {
                current_lsn: durable.current_lsn,
                committed_lsn: durable.policy_certified_lsn,
                ..Default::default()
            })
        }
    }

    async fn set_access_status(&self, read: AccessStatus, write: AccessStatus) -> Result<()> {
        let state = self.state.lock().await;
        self.validate().await?;
        if read == AccessStatus::Granted
            && write != AccessStatus::Granted
            && state.role == ReplicaRole::ActiveSecondary
        {
            let build = self
                .durable
                .snapshot()
                .await
                .native_build
                .ok_or(RuntimeError::ReconfigurationPending)?;
            if build.stage != PgBuildStage::Complete || !self.instance.is_running().await {
                return self.close_access().await;
            }
            let authority = state
                .authority
                .as_ref()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            if !authority.current_configuration.members.iter().any(|m| {
                m.identity == authority.local_identity && m.role == ReplicaRole::ActiveSecondary
            }) {
                return Err(RuntimeError::AuthorityNotAdmitted);
            }
            self.pg_result(
                PgAccessController::new(&self.instance)
                    .grant_role_access()
                    .await,
            )
            .await?;
            self.durable
                .update(|state| {
                    state.external_access_closed = false;
                    Ok(())
                })
                .await
                .map_err(application_error)?;
            return Ok(());
        }
        if read != AccessStatus::Granted || write != AccessStatus::Granted {
            return self.close_access().await;
        }
        let authority = state
            .authority
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if state.role != ReplicaRole::Primary || !self.instance.is_running().await {
            return self.close_access().await;
        }
        let snapshot = self.observe_pg().await?;
        let evidence = snapshot.evidence.ok_or(RuntimeError::NotPrimary)?;
        let Ok(sessions) = self
            .current_sessions(&authority.current_configuration)
            .await
        else {
            return self.close_access().await;
        };
        let Ok(expected) = compile_synchronous_configuration(
            None,
            &authority.current_configuration,
            &authority.local_identity,
            &sessions,
            true,
        ) else {
            return self.close_access().await;
        };
        if evidence.in_recovery || evidence.synchronous.as_ref() != Some(&expected) {
            return self.close_access().await;
        }
        PgAccessController::new(&self.instance)
            .grant_role_access()
            .await
            .map_err(application_error)?;
        if let Err(error) = self
            .durable
            .update(|state| {
                state.external_access_closed = false;
                Ok(())
            })
            .await
        {
            let _ = self.close_access().await;
            return Err(self.permanent(error).await);
        }
        Ok(())
    }

    async fn install_configuration(&self, current: ReplicaSetConfiguration) -> Result<()> {
        kuberic_protocol::validation::validate_configuration(&current.configuration, None)
            .map_err(application_error)?;
        let old = self.configuration.read().await.clone();
        if old.as_ref() == Some(&current) {
            return Ok(());
        }
        if old
            .as_ref()
            .is_some_and(|old| old.configuration.epoch > current.configuration.epoch)
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.build_cancellation.lock().unwrap().cancel();
        let mut state = self.state.lock().await;
        if self.cancellation.is_cancelled() {
            return Err(RuntimeError::Closed);
        }
        let durable = self.validate().await?;
        let local = &durable.identity.replica;
        let local_session = current
            .replicas
            .iter()
            .find(|r| &r.identity == local && !r.process_session_id.is_empty())
            .ok_or(RuntimeError::AuthorityNotAdmitted)?
            .process_session_id
            .clone();
        self.close_access().await?;
        let member = current
            .configuration
            .members
            .iter()
            .find(|m| &m.identity == local);
        state.authority = member.map(|_| PgConfiguration {
            local_identity: local.clone(),
            current_configuration: current.configuration.clone(),
        });
        *self.configuration.write().await = Some(current.clone());
        *self.build_cancellation.lock().unwrap() = CancellationToken::new();
        if member.is_some() {
            self.durable
                .update(|state| {
                    state.has_accepted_authority = true;
                    Ok(())
                })
                .await
                .map_err(application_error)?;
        }
        if let Some(build) = &durable.native_build {
            let primary = current
                .configuration
                .members
                .iter()
                .find(|m| m.role == ReplicaRole::Primary)
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            let admitted_standby = member.is_some_and(|m| m.role == ReplicaRole::ActiveSecondary)
                && build.stage == PgBuildStage::Complete
                && primary.identity == build.request.authority.source;
            if !admitted_standby
                && !current
                    .replicas
                    .iter()
                    .any(|r| r.identity == *local && r.build_id == build.request.authority.build_id)
            {
                self.stop().await?;
            }
            if admitted_standby
                && self.peer_session(&primary.identity).await.is_ok()
                && !self.instance.is_running().await
            {
                self.pg_result(
                    self.instance
                        .config()
                        .configure_standby(
                            self.instance.data_dir(),
                            &build.request.source_host,
                            build.request.source_port,
                            &crate::native::replication_application_name(local, &local_session),
                            &build.request.lineage,
                        )
                        .await,
                )
                .await?;
                self.pg_result(
                    self.instance
                        .start_native_with_cancellation(
                            self.fault_tx.clone(),
                            self.cancellation.clone(),
                        )
                        .await,
                )
                .await?;
                self.observe_pg().await?;
            }
        }
        if member.is_none_or(|m| m.role != ReplicaRole::Primary)
            || !self.instance.is_running().await
        {
            return Ok(());
        }
        let sessions = self
            .current_sessions(&current.configuration)
            .await
            .unwrap_or_default();
        let invalid = crate::native::AcknowledgementPolicy {
            configuration_generation: u64::try_from(
                current.configuration.epoch.configuration_number,
            )
            .map_err(application_error)?,
            configuration_id: current.configuration.configuration_id.clone(),
            valid: false,
            write_acknowledgements: 0,
            eligible_standbys: Vec::new(),
        };
        self.pg_result(self.observer.set_synchronous(invalid).await)
            .await?;
        let synchronous =
            compile_synchronous_configuration(None, &current.configuration, local, &sessions, true);
        // Peer discovery follows process reconstruction. Incomplete current
        // sessions leave access closed, not a failed restart or an old quorum.
        let Ok(synchronous) = synchronous else {
            return Ok(());
        };
        self.pg_result(self.observer.apply_synchronous(synchronous).await)
            .await
    }

    async fn wait_for_catch_up_quorum(&self, _: ReplicaSetQuorumMode) -> Result<()> {
        let _state = self.state.lock().await;
        if self.cancellation.is_cancelled() {
            return Err(RuntimeError::Closed);
        }
        let snapshot = self.observe_pg().await?;
        let configuration = evidence_configuration(&snapshot)
            .cloned()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let stored = self.durable.snapshot().await.catch_up;
        let boundary = match stored {
            Some((id, boundary)) if id == configuration => boundary,
            _ => {
                let boundary = snapshot
                    .evidence
                    .as_ref()
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?
                    .flush_lsn;
                self.durable
                    .update(|state| {
                        state.catch_up = Some((configuration.clone(), boundary));
                        Ok(())
                    })
                    .await
                    .map_err(application_error)?;
                boundary
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            loop {
                if self.cancellation.is_cancelled() {
                    return Err(RuntimeError::OperationCancelled);
                }
                let snapshot = self.observe_pg().await?;
                if evidence_configuration(&snapshot) != Some(&configuration) {
                    return Err(RuntimeError::AuthorityNotAdmitted);
                }
                if snapshot.committed_lsn >= boundary {
                    return Ok(());
                }
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .map_err(|_| application_error("native quorum replay timed out"))?
    }
    async fn retire_build(&self, id: &OperationId) -> Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(RuntimeError::Closed);
        }
        self.build_cancellation.lock().unwrap().cancel();
        let state = self.state.lock().await;
        let durable = self.durable.snapshot().await;
        if durable
            .native_build
            .as_ref()
            .is_some_and(|build| &build.request.authority.build_id == id)
            && state.authority.as_ref().is_none_or(|a| {
                !a.current_configuration.members.iter().any(|m| {
                    m.identity == a.local_identity && m.role == ReplicaRole::ActiveSecondary
                })
            })
        {
            self.instance.stop().await.map_err(application_error)?;
        }
        self.durable
            .update(|state| {
                state
                    .outbound_builds
                    .retain(|b| &b.request.authority.build_id != id);
                Ok(())
            })
            .await
            .map_err(application_error)?;
        Ok(())
    }
    async fn on_data_loss(&self) -> Result<bool> {
        unsupported("data loss")
    }
    async fn execute_build(&self, replica: ReplicaInformation) -> Result<()> {
        let endpoint = replica.replication_address.clone();
        if !endpoint.starts_with("http://") {
            return Err(application_error(
                "missing exact target replication endpoint",
            ));
        }
        let request = {
            let _state = self.state.lock().await;
            if self.cancellation.is_cancelled() {
                return Err(RuntimeError::Closed);
            }
            *self.build_cancellation.lock().unwrap() = CancellationToken::new();
            let durable = self.durable.revalidate().await.map_err(application_error)?;
            let configuration = self
                .configuration
                .read()
                .await
                .clone()
                .ok_or(RuntimeError::AuthorityNotAdmitted)?;
            let authority = BuildAuthority {
                build_id: replica.build_id.clone(),
                kind: if configuration
                    .configuration
                    .members
                    .iter()
                    .any(|m| m.identity == replica.identity)
                {
                    kuberic_protocol::types::BuildAuthorityKind::Bootstrap
                } else {
                    kuberic_protocol::types::BuildAuthorityKind::Provisioning
                },
                source: durable.identity.replica.clone(),
                target: replica.identity.clone(),
                current_configuration: configuration.configuration,
                replication_boundary_lsn: replica.current_progress,
            };
            let snapshot = self.observe_pg().await?;
            let lineage = self.lineage().await?;
            if snapshot.evidence.is_none_or(|e| e.in_recovery)
                || snapshot.committed_lsn < authority.replication_boundary_lsn
            {
                return Err(RuntimeError::ReconfigurationPending);
            }
            let request = PgBuildRequest {
                version: BUILD_PROTOCOL_VERSION,
                resource_uid: durable.identity.resource_uid.clone(),
                authority,
                source_session: self.peer_session(&durable.identity.replica).await?,
                target_session: replica.process_session_id.clone(),
                source_endpoint: self.coordination.local_endpoint.clone(),
                source_host: self.instance.listen_host().into(),
                source_port: self.instance.port(),
                lineage,
            };
            self.validate_build(&request).await?;
            if let Some(previous) = durable
                .outbound_builds
                .iter()
                .find(|b| b.request.authority.build_id == request.authority.build_id)
                && !previous.request.same_work(&request)
            {
                return Err(application_error("source build lineage changed"));
            }
            self.durable
                .update(|state| {
                    state
                        .outbound_builds
                        .retain(|b| b.request.authority.build_id != request.authority.build_id);
                    if state.outbound_builds.len() >= MAX_BUILDS {
                        return Err(crate::durable::PgDurableError::Invalid(
                            "native build capacity exhausted".into(),
                        ));
                    }
                    state.outbound_builds.push(PgBuildProgress {
                        request: request.clone(),
                        stage: PgBuildStage::Intent,
                        method: PgBuildMethod::Fresh,
                        sequence: 1,
                        evidence: None,
                    });
                    Ok(())
                })
                .await
                .map_err(application_error)?;
            request
        };
        let result = tokio::time::timeout(std::time::Duration::from_secs(65), async {
            let mut client =
                crate::proto::pg_data_service_client::PgDataServiceClient::connect(endpoint)
                    .await
                    .map_err(application_error)?;
            client
                .build(self.rpc_request(&request)?)
                .await
                .map_err(application_error)
        })
        .await
        .map_err(|_| application_error("native target build timed out"))??;
        let progress: PgBuildProgress =
            decode(&result.into_inner().progress_json).map_err(application_error)?;
        progress.validate().map_err(application_error)?;
        let _state = self.state.lock().await;
        self.validate_build(&request).await?;
        if progress.request != request || progress.stage != PgBuildStage::Complete {
            return Err(application_error(
                "native target returned non-exact completion",
            ));
        }
        self.durable
            .update(|state| {
                let entry = state
                    .outbound_builds
                    .iter_mut()
                    .find(|b| b.request == request)
                    .ok_or_else(|| {
                        crate::durable::PgDurableError::Invalid("native build was replaced".into())
                    })?;
                *entry = progress;
                Ok(())
            })
            .await
            .map_err(application_error)?;
        Ok(())
    }
}

#[async_trait]
impl Replicator for PgReplicator {
    async fn open(&self) -> Result<String> {
        PgReplicator::open(self).await?;
        Ok(self.coordination.local_endpoint.clone())
    }
    async fn change_role(&self, epoch: Epoch, role: ReplicaRole) -> Result<()> {
        PgReplicator::change_role(self, epoch, role).await
    }
    async fn update_epoch(&self, epoch: Epoch) -> Result<()> {
        self.build_cancellation.lock().unwrap().cancel();
        PgReplicator::update_epoch(self, epoch).await
    }
    async fn close(&self) -> Result<()> {
        PgReplicator::close(self).await
    }
    fn abort(&self) {
        PgReplicator::abort(self);
    }
    async fn current_progress(&self) -> Result<i64> {
        self.validate().await?;
        let partition = self.partition.upgrade().ok_or(RuntimeError::Closed)?;
        self.set_access_status(
            partition.get_read_status().await?,
            partition.get_write_status().await?,
        )
        .await?;
        let durable = self.durable.snapshot().await;
        if self.state.lock().await.role == ReplicaRole::IdleSecondary {
            let Some(build) = &durable.native_build else {
                return Ok(0);
            };
            if build.stage != PgBuildStage::Complete
                || self.validate_build(&build.request).await.is_err()
            {
                return Ok(0);
            }
        }
        if !self.instance.is_running().await {
            return Ok(0);
        }
        Ok(self.observe().await?.committed_lsn)
    }
    async fn catch_up_capability(&self) -> Result<i64> {
        self.current_progress().await
    }
}

#[async_trait]
impl PrimaryReplicator for PgReplicator {
    async fn on_data_loss(&self) -> Result<bool> {
        PgReplicator::on_data_loss(self).await
    }
    async fn update_catch_up_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
        _: ReplicaSetConfiguration,
    ) -> Result<()> {
        unsupported("reconfiguration")
    }
    async fn update_current_replica_set_configuration(
        &self,
        current: ReplicaSetConfiguration,
    ) -> Result<()> {
        self.install_configuration(current).await
    }
    async fn wait_for_catch_up_quorum(&self, mode: ReplicaSetQuorumMode) -> Result<()> {
        PgReplicator::wait_for_catch_up_quorum(self, mode).await
    }
    async fn build_replica(&self, replica: ReplicaInformation) -> Result<()> {
        self.execute_build(replica).await
    }
    async fn remove_replica(&self, replica_id: ReplicaId) -> Result<()> {
        if self.cancellation.is_cancelled() {
            return Err(RuntimeError::Closed);
        }
        let durable = self.durable.snapshot().await;
        let ids = durable
            .native_build
            .iter()
            .chain(durable.outbound_builds.iter())
            .filter(|b| b.request.authority.target.replica_id == replica_id)
            .map(|b| b.request.authority.build_id.clone())
            .collect::<Vec<_>>();
        for id in ids {
            self.retire_build(&id).await?;
        }
        Ok(())
    }
}
