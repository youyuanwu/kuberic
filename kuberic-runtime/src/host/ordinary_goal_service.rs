use super::*;
use crate::protocol::public_operations::*;
use crate::protocol::types::ReplicaRole;

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
        if !snapshot.withdrawn {
            // Only observations for this exact live creation are current. Old
            // journal history remains stored, but cannot recreate a warm owner.
            if let Some(scope) = snapshot
                .goals
                .iter()
                .filter(|observation| {
                    observation.goal.scope.process_session_id == snapshot.process_session_id
                        && observation.goal.scope.creation_id == snapshot.creation_id
                })
                .max_by_key(|observation| observation.goal.revision)
                .map(|observation| &observation.goal.scope)
            {
                snapshot.native_status = crate::host::native_history::status_for(&state, scope)
                    .filter(|status| self.runtime.native_status_is_live(status));
                let prefix = self
                    .runtime
                    .native_configuration_prefix(&state, scope)
                    .filter(|facts| self.runtime.native_facts_are_live(facts));
                if let Some(facts) = &prefix {
                    snapshot.native_status =
                        crate::host::native_history::configuration_status(&state, facts);
                }
                snapshot.native_lifecycle = prefix
                    .or_else(|| crate::host::native_history::lifecycle_for(&state, scope))
                    .filter(|facts| self.runtime.native_facts_are_live(facts));
            }
        }
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

    pub(super) async fn sampled_ordinary_snapshot(
        &self,
    ) -> std::result::Result<ReplicaAgentSnapshot, Status> {
        let snapshot = self.ordinary_snapshot().await?;
        if self.runtime.has_native_lifecycle() {
            let mut snapshot = snapshot;
            if let Some(status) = &snapshot.native_status {
                snapshot.progress = status.progress.clone();
                snapshot.capability = status.capability.clone();
            }
            // Native epoch/status is an explicitly selected Discover program.
            // Sampling metadata must never hide an unjournaled epoch effect.
            return Ok(snapshot);
        }
        let Some(publication) = snapshot.publication.as_ref().filter(|publication| {
            publication.stage == ReplicaPublicationStage::AchievedScope
                && matches!(
                    publication.role,
                    ReplicaRole::Primary | ReplicaRole::ActiveSecondary
                )
        }) else {
            return Ok(snapshot);
        };
        let (progress, capability) = self
            .runtime
            .ordinary_native_progress()
            .await
            .map_err(status_from_runtime)?;
        let mut current = self.ordinary_snapshot().await?;
        if current.publication.as_ref() == Some(publication) && !current.withdrawn {
            current.progress = progress;
            current.capability = capability;
        }
        Ok(current)
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

    fn validate_goal_creation(&self, goal: &ReplicaGoal) -> std::result::Result<(), Status> {
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
        Ok(())
    }

    async fn validate_goal_target(&self, goal: &ReplicaGoal) -> std::result::Result<(), Status> {
        self.validate_goal_creation(goal)?;
        let state = self.store.load_state().await.map_err(status_from_agent)?;
        crate::host::ordinary_goal::validate_admission(&state, goal).map_err(status_from_agent)?;
        crate::host::ordinary_goal::validate_live_normal_current(&state, goal, |facts| {
            self.runtime.native_facts_are_live(facts)
        })
        .map_err(status_from_agent)?;
        if crate::host::ordinary_goal::normal_current_owner(&state, goal).is_some() {
            let callbacks = self
                .runtime
                .public_lifecycle_callbacks()
                .map_err(status_from_runtime)?;
            crate::host::native_history::validate_normal_current_source(&state, goal, &callbacks)
                .await
                .map_err(status_from_agent)?;
        }
        if let ReplicaGoalKind::Prepare {
            rebuild: Some(rebuild),
            ..
        } = &goal.desired
        {
            let source = &rebuild.source;
            let address = self
                .runtime
                .peer_control_endpoint(&source.scope.storage.replica)
                .map_err(status_from_runtime)?;
            let snapshot = crate::control::resolve_native_snapshot(
                &address,
                &self.bearer_token,
                &source.scope.history,
                &source.scope.storage.replica,
            )
            .await
            .map_err(status_from_runtime)?;
            if snapshot.withdrawn
                || snapshot.publication.as_ref() != Some(source)
                || snapshot.native_lifecycle.as_ref().is_none_or(|facts| {
                    facts.scope != source.scope
                        || facts.state != NativeReplicaState::Ready
                        || facts.native_role != ReplicaRole::Primary
                        || facts.application_role != Some(ReplicaRole::Primary)
                        || facts.applied_primary_fence != Some(source.scope.epoch)
                })
            {
                return Err(Status::failed_precondition(
                    "rebuild source is not actual configured Ready Primary",
                ));
            }
        }
        if let Some(addition) = crate::host::ordinary_goal::addition::selection(goal)
            .filter(|addition| addition.phase == MembershipAdditionPhase::Bootstrap)
        {
            for settlement in &crate::host::ordinary_goal::addition::recovery(goal)
                .unwrap()
                .receiver_settlements
            {
                let address = self
                    .runtime
                    .peer_control_endpoint(&settlement.terminal_goal.scope.storage.replica)
                    .map_err(status_from_runtime)?;
                let receiver = crate::control::resolve_receiver_settlement(
                    &address,
                    &self.bearer_token,
                    settlement,
                )
                .await
                .map_err(status_from_runtime)?;
                if !crate::host::ordinary_goal::reconstruction::receiver_settled(
                    settlement, &receiver,
                ) || receiver.storage != addition.receiver.storage_incarnation
                    || receiver.process_session_id != addition.receiver.process_session_id
                    || receiver.creation_id != addition.receiver.replicator_creation_id
                {
                    return Err(Status::failed_precondition(
                        "addition selection lacks exact settled new receiver",
                    ));
                }
            }
        }
        if let Some(addition) = crate::host::ordinary_goal::addition::selection(goal)
            .filter(|addition| addition.phase == MembershipAdditionPhase::Receiver)
        {
            let source = &addition.source;
            let address = self
                .runtime
                .peer_control_endpoint(&source.identity)
                .map_err(status_from_runtime)?;
            let observed = crate::control::resolve_replica_publication(
                &address,
                &self.bearer_token,
                &ReplicaDiscoveryQuery {
                    format: ORDINARY_ACTION_FORMAT,
                    history: goal.scope.history.clone(),
                    replica: source.identity.clone(),
                    process_session_id: Some(source.process_session_id.clone()),
                },
            )
            .await
            .map_err(status_from_runtime)?;
            if !observed.is_some_and(|publication| {
                publication.stage == ReplicaPublicationStage::AchievedScope
                    && publication.role == ReplicaRole::Primary
                    && publication.scope.storage == source.storage_incarnation
                    && publication.scope.creation_id == source.replicator_creation_id
                    && publication.scope.configuration == goal.scope.configuration
            }) {
                return Err(Status::failed_precondition(
                    "addition receiver requires actual exact achieved P0 source",
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn submit_ordinary_goal(
        &self,
        request: proto::ReplicaGoalRequest,
    ) -> std::result::Result<proto::ReplicaSnapshotResponse, Status> {
        version(request.protocol_version)?;
        let goal: ReplicaGoal = decode(&request.canonical_json)?;
        if std::env::var_os("KUBERIC_M3_NATIVE_SCOPE_DIAGNOSTICS").is_some() {
            eprintln!(
                "ordinary actual submitted goal: {}",
                serde_json::to_string(&goal).unwrap()
            );
        }
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
            ReplicaGoalKind::Discover
                | ReplicaGoalKind::Prepare { .. }
                | ReplicaGoalKind::RestoreRetainedMember { .. }
                | ReplicaGoalKind::RecoverContainedCreation { .. }
        ) {
            let state = self.store.load_state().await.map_err(status_from_agent)?;
            let mut context = self
                .runtime
                .public_open_context(state.application_open_mode())
                .await;
            if matches!(goal.desired, ReplicaGoalKind::RestoreRetainedMember { .. }) {
                if context.mode != crate::application::OpenMode::Existing {
                    return Err(Status::failed_precondition(
                        "retained restoration cannot initialize storage",
                    ));
                }
                context.partition = context.partition.with_retained_member_instructions();
            }
            if crate::host::ordinary_goal::reconstruction::claim(&goal).is_some() {
                if context.mode != crate::application::OpenMode::Existing {
                    return Err(Status::failed_precondition(
                        "contained reconstruction cannot initialize storage",
                    ));
                }
                context.partition = context
                    .partition
                    .with_contained_reconstruction_instructions();
            }
            callbacks.open_context = Some(context);
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
        remote.validate().map_err(Status::invalid_argument)?;
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
        if publication.as_ref() != Some(&remote.target) && remote.original_member.is_none() {
            return Err(Status::failed_precondition(
                "remote target publication changed",
            ));
        }
        if remote.original_member.is_some() {
            let snapshot = crate::control::resolve_native_snapshot(
                &remote.target_control_address,
                &self.bearer_token,
                &remote.target.scope.history,
                &remote.target.scope.storage.replica,
            )
            .await
            .map_err(status_from_runtime)?;
            let state = self.store.load_state().await.map_err(status_from_agent)?;
            if !crate::host::original_member::target_admitted(
                remote,
                &snapshot,
                crate::host::original_member::accepted_build(&state, remote),
            ) {
                return Err(Status::failed_precondition(
                    "original-member target lost exact actual Idle preparation",
                ));
            }
        }
        Ok(())
    }

    pub(super) async fn acknowledge_ordinary_goal(
        &self,
        request: proto::ReplicaGoalRequest,
    ) -> std::result::Result<proto::ReplicaSnapshotResponse, Status> {
        version(request.protocol_version)?;
        let acknowledgement: ReplicaGoalAcknowledgement = decode(&request.canonical_json)?;
        self.validate_goal_creation(&acknowledgement.goal)?;
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
        if crate::host::ordinary_goal::addition::selection(&acknowledgement.goal)
            .is_some_and(|addition| addition.phase == MembershipAdditionPhase::Current)
        {
            self.runtime
                .accept_addition_access(self.store.as_ref(), &acknowledgement.goal)
                .await
                .map_err(status_from_agent)?;
        }
        let state = self.store.load_state().await.map_err(status_from_agent)?;
        let goal = &acknowledgement.goal;
        if crate::host::original_member::successful_final(&state, goal) {
            let mut settled = true;
            for member in goal
                .scope
                .configuration
                .current
                .members
                .iter()
                .filter(|member| member.identity != goal.scope.storage.replica)
            {
                let address = self
                    .runtime
                    .peer_control_endpoint(&member.identity)
                    .map_err(status_from_runtime)?;
                let peer = crate::control::resolve_native_snapshot(
                    &address,
                    &self.bearer_token,
                    &goal.scope.history,
                    &member.identity,
                )
                .await
                .map_err(status_from_runtime)?;
                settled &= crate::host::original_member::final_peer(goal, &peer);
            }
            if settled {
                self.store
                    .finish_original_member_rebuild(goal)
                    .await
                    .map_err(status_from_agent)?;
                if goal.role() == Some(ReplicaRole::Primary) {
                    self.runtime
                        .accept_addition_access(self.store.as_ref(), goal)
                        .await
                        .map_err(status_from_agent)?;
                }
            }
        }
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
