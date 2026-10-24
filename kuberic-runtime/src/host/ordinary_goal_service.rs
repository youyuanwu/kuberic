use super::*;
use crate::protocol::public_operations::*;

fn version(format: u32) -> std::result::Result<(), Status> {
    if format != ORDINARY_ACTION_FORMAT {
        return Err(Status::failed_precondition(
            "incompatible ordinary hosting format",
        ));
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> std::result::Result<T, Status> {
    serde_json::from_slice(bytes).map_err(|error| Status::invalid_argument(error.to_string()))
}

fn response<T: serde::Serialize>(
    value: &T,
) -> std::result::Result<proto::ReplicaSnapshotResponse, Status> {
    Ok(proto::ReplicaSnapshotResponse {
        protocol_version: ORDINARY_ACTION_FORMAT,
        canonical_json: serde_json::to_vec(value)
            .map_err(|error| Status::internal(error.to_string()))?,
    })
}

impl<S: AgentStore + 'static> AgentService<S> {
    pub(super) async fn ordinary_snapshot(
        &self,
    ) -> std::result::Result<ReplicaAgentSnapshot, Status> {
        let state = self.store.load_state().await.map_err(status_from_agent)?;
        if !state
            .public_v1_operation_state
            .as_ref()
            .is_some_and(|preview| {
                preview.history.is_some() && preview.storage_incarnation.is_some()
            })
        {
            return Err(Status::failed_precondition(
                "snapshot requires initialized exact history/storage",
            ));
        }
        let creation = self
            .runtime
            .replacement_replicator_creation_id()
            .map_err(status_from_runtime)?;
        let mut snapshot = crate::host::ordinary_goal::snapshot(
            &state,
            self.session.id().clone(),
            creation,
            self.runtime.ordinary_withdrawn(),
        );
        let (_, _, _, read, write, _, _) = self.runtime.diagnostic_projection().await;
        snapshot.read_status = read;
        snapshot.write_status = write;
        snapshot.provider_writer_held = self.runtime.provider_writer_lease_count() != 0;
        for remote in &mut snapshot.remote_operations {
            if remote.state == RemoteReplicaState::Ready
                && self.validate_remote_peer(&remote.selection).await.is_err()
            {
                remote.state = RemoteReplicaState::Uncertain;
            }
        }
        Ok(snapshot)
    }

    pub(super) async fn ordinary_snapshot_response(
        &self,
    ) -> std::result::Result<proto::ReplicaSnapshotResponse, Status> {
        response(&self.ordinary_snapshot().await?)
    }

    pub(super) async fn validate_snapshot_request(
        &self,
        request: &proto::ReplicaSnapshotRequest,
    ) -> std::result::Result<(), Status> {
        version(request.protocol_version)?;
        let state = self.store.load_state().await.map_err(status_from_agent)?;
        let target = request
            .target
            .as_ref()
            .ok_or_else(|| Status::invalid_argument("missing exact snapshot target"))?;
        if request.resource_uid != state.identity.resource_uid.as_str()
            || target.replica_id != state.identity.local_identity.replica_id.value()
            || target.instance_id != state.identity.local_identity.instance_id.as_str()
            || target.agent_generation != state.identity.local_identity.agent_generation.as_str()
            || (!request.expected_process_session_id.is_empty()
                && request.expected_process_session_id != self.session.id().as_str())
        {
            return Err(Status::failed_precondition(
                "snapshot targets another process or replica",
            ));
        }
        Ok(())
    }

    async fn validate_goal_target(&self, goal: &ReplicaGoal) -> std::result::Result<(), Status> {
        goal.validate().map_err(Status::invalid_argument)?;
        if &goal.scope.process_session_id != self.session.id()
            || goal.scope.creation_id
                != self
                    .runtime
                    .replacement_replicator_creation_id()
                    .map_err(status_from_runtime)?
        {
            return Err(Status::failed_precondition(
                "goal targets a stale process or creation",
            ));
        }
        let state = self.store.load_state().await.map_err(status_from_agent)?;
        crate::host::ordinary_goal::validate_admission(&state, goal).map_err(status_from_agent)
    }

    pub(super) async fn submit_ordinary_goal(
        &self,
        request: proto::ReplicaGoalRequest,
    ) -> std::result::Result<proto::ReplicaSnapshotResponse, Status> {
        version(request.protocol_version)?;
        let goal: ReplicaGoal = decode(&request.canonical_json)?;
        self.validate_goal_target(&goal).await?;
        let registry = self
            .registry
            .get()
            .cloned()
            .ok_or_else(|| Status::unavailable("ordinary operation owner unavailable"))?;
        let service = self.clone();
        let (completed, result) = tokio::sync::oneshot::channel();
        registry
            .try_own_control_task(async move {
                let outcome = service.submit_ordinary_goal_owned(goal).await;
                let _ = completed.send(outcome);
            })
            .map_err(status_from_agent)?;
        result
            .await
            .map_err(|_| Status::unavailable("retained goal dispatch owner was lost"))?
    }

    async fn submit_ordinary_goal_owned(
        &self,
        goal: ReplicaGoal,
    ) -> std::result::Result<proto::ReplicaSnapshotResponse, Status> {
        self.validate_goal_target(&goal).await?;
        if self.runtime.ordinary_withdrawn() && !goal.class().is_terminal() {
            return Err(Status::failed_precondition(
                "ordinary creation has been withdrawn and requires process replacement",
            ));
        }
        let registry = self
            .registry
            .get()
            .cloned()
            .ok_or_else(|| Status::unavailable("ordinary operation owner unavailable"))?;
        self.runtime
            .enable_ordinary_contract()
            .map_err(status_from_runtime)?;
        if let Some(remote) = goal.remote()
            && matches!(goal.desired, ReplicaGoalKind::Build { .. })
        {
            self.validate_remote_peer(remote).await?;
        }
        if matches!(goal.desired, ReplicaGoalKind::Reconfigure { .. }) {
            let state = self.store.load_state().await.map_err(status_from_agent)?;
            if !state
                .public_v1_operation_state
                .as_ref()
                .is_some_and(|preview| preview.operations.contains_key(&goal.operation_id))
            {
                for remote in crate::host::ordinary_goal::admission_ready(&state, &goal)
                    .map_err(status_from_agent)?
                {
                    self.validate_remote_peer(&remote).await?;
                }
            }
        }
        let mut callbacks = self
            .runtime
            .public_lifecycle_callbacks()
            .map_err(status_from_runtime)?;
        if matches!(
            goal.desired,
            ReplicaGoalKind::Discover | ReplicaGoalKind::Prepare { .. }
        ) {
            let state = self.store.load_state().await.map_err(status_from_agent)?;
            callbacks.open_context = Some(
                self.runtime
                    .public_open_context(state.application_open_mode())
                    .await,
            );
        }
        let intent = goal.intent();
        let (operation, attempt, mut access_guard) =
            crate::host::public_lifecycle::launch_bound_attempt(
                &registry,
                self.store.clone(),
                intent,
                callbacks,
                |operation| self.runtime.public_access_completion_guard(operation),
            )
            .await
            .map_err(status_from_agent)?;
        #[cfg(all(test, feature = "testing"))]
        registry
            .pause_completion_registration(&goal.operation_id)
            .await;
        let service = self.clone();
        // The RPC owns no ordinary future. Completion, failed preparation and
        // withdrawal are retained even when every response observer disappears.
        registry.own_control_task(async move {
            let terminal = operation.wait_for_attempt(attempt).await;
            if let Ok(record) = terminal
                && record.stage == PublicOperationStage::Completed
                && record.disposition == Some(PublicOperationDisposition::Succeeded)
            {
                service.runtime.complete_public_access(&operation);
                if matches!(
                    goal.desired,
                    ReplicaGoalKind::Discover | ReplicaGoalKind::Prepare { .. }
                ) && let Ok(state) = service.store.load_state().await
                    && state
                        .application_storage
                        .as_ref()
                        .is_some_and(|binding| binding.initializing)
                {
                    let _ = service.store.complete_application_initialization().await;
                }
                access_guard.succeeded();
                return;
            }
            service.runtime.withdraw_ordinary();
            if let Some(registry) = service.registry.get() {
                registry.fence();
                if let Ok(callbacks) = service.runtime.public_lifecycle_callbacks()
                    && let Err(error) = crate::host::public_lifecycle::recover_terminal(
                        registry, &operation, callbacks,
                    )
                    .await
                {
                    operation.record_containment_failure(&error);
                }
            }
        });
        self.ordinary_snapshot_response().await
    }

    async fn validate_remote_peer(
        &self,
        remote: &RemoteReplicaSelection,
    ) -> std::result::Result<(), Status> {
        if self
            .runtime
            .peer_control_endpoint(&remote.target.scope.storage.replica)
            .map_err(status_from_runtime)?
            != remote.target_control_address
        {
            return Err(Status::failed_precondition(
                "remote control address differs from host resolver",
            ));
        }
        let query = ReplicaDiscoveryQuery {
            format: ORDINARY_ACTION_FORMAT,
            history: remote.target.scope.history.clone(),
            replica: remote.target.scope.storage.replica.clone(),
            process_session_id: Some(remote.target.scope.process_session_id.clone()),
        };
        let publication = crate::control::resolve_replica_publication(
            &remote.target_control_address,
            &self.bearer_token,
            &query,
        )
        .await
        .map_err(status_from_runtime)?;
        if publication.as_ref() != Some(&remote.target) {
            return Err(Status::failed_precondition(
                "remote target publication changed",
            ));
        }
        Ok(())
    }

    pub(super) async fn acknowledge_ordinary_goal(
        &self,
        request: proto::ReplicaGoalRequest,
    ) -> std::result::Result<proto::ReplicaSnapshotResponse, Status> {
        version(request.protocol_version)?;
        let acknowledgement: ReplicaGoalAcknowledgement = decode(&request.canonical_json)?;
        self.validate_goal_target(&acknowledgement.goal).await?;
        let state = self.store.load_state().await.map_err(status_from_agent)?;
        crate::host::ordinary_goal::validate_acknowledgement(&state, &acknowledgement)
            .map_err(status_from_agent)?;
        let succeeded = state
            .public_v1_operation_state
            .as_ref()
            .and_then(|preview| preview.operations.get(&acknowledgement.goal.operation_id))
            .is_some_and(|record| {
                record.stage == PublicOperationStage::Completed
                    && record.disposition == Some(PublicOperationDisposition::Succeeded)
            });
        if let Some(remote) = acknowledgement.goal.remote()
            && succeeded
            && matches!(acknowledgement.goal.desired, ReplicaGoalKind::Build { .. })
            && !acknowledgement.logically_retired
        {
            self.validate_remote_peer(remote).await?;
            let snapshot = self.ordinary_snapshot().await?;
            if snapshot.publication.as_ref() != Some(&remote.source) {
                return Err(Status::failed_precondition(
                    "source publication changed before acceptance",
                ));
            }
        }
        self.registry
            .get()
            .ok_or_else(|| Status::unavailable("ordinary goal owner unavailable"))?
            .join_ordinary_goal(&acknowledgement.goal.operation_id)
            .await
            .map_err(status_from_agent)?;
        self.store
            .acknowledge_ordinary_goal(&acknowledgement)
            .await
            .map_err(status_from_agent)?;
        self.ordinary_snapshot_response().await
    }

    pub(super) async fn discover_ordinary_peer(
        &self,
        request: proto::ReplicaDiscoveryRequest,
    ) -> std::result::Result<proto::ReplicaSnapshotResponse, Status> {
        version(request.protocol_version)?;
        let query: ReplicaDiscoveryQuery = decode(&request.canonical_json)?;
        version(query.format)?;
        let state = self.store.load_state().await.map_err(status_from_agent)?;
        let preview = state
            .public_v1_operation_state
            .as_ref()
            .ok_or_else(|| Status::failed_precondition("missing ordinary state"))?;
        if query.history
            != *preview
                .history
                .as_ref()
                .ok_or_else(|| Status::failed_precondition("missing accepted history"))?
            || query.replica != state.identity.local_identity
            || query
                .process_session_id
                .as_ref()
                .is_some_and(|expected| expected != self.session.id())
        {
            return Err(Status::failed_precondition(
                "discovery targets another scope",
            ));
        }
        let creation = self
            .runtime
            .replacement_replicator_creation_id()
            .map_err(status_from_runtime)?;
        response(&crate::host::ordinary_goal::publication(
            &state,
            self.session.id(),
            &creation,
            self.runtime.ordinary_withdrawn(),
        ))
    }
}
