use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use kuberic_protocol::types::{AccessStatus, OperationId, ReplicaIdentity, ReplicaRole};
use kuberic_wire::{
    normalize_copy_ack, normalize_copy_item, normalize_replication_ack, normalize_replication_item,
    proto,
};
use tokio::sync::{Mutex, RwLock, oneshot};

use crate::application::{
    ClientWrite, DurableApplicationAck, OpenContext, Operation, StatefulApplication, WriteReceipt,
};
use crate::authority::{
    AdmittedAuthority, AuthorityStore, BuildAuthority, BuildAuthorityKind, DurableBuildProgress,
    DurableLocalWrite, LocalWritePhase, ReplicationProgress,
};
use crate::effects::{
    BuildPostcondition, RuntimeControlPlane, RuntimeEffect, RuntimeEffectAction,
    RuntimeEffectResult, RuntimePostcondition,
};
use crate::replicator::copy::{
    BuildConfiguration, BuildProgress, PrepareCopyRequest, PreparedCopy,
};
use crate::replicator::{PreparedWrite, Replicator};
use crate::{Result, RuntimeError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub identity: ReplicaIdentity,
    pub open: bool,
    pub role: ReplicaRole,
    pub write_status: AccessStatus,
    pub authority: Option<AdmittedAuthority>,
    pub current_progress: i64,
    pub verified_replication_lsn: Option<i64>,
    pub committed_lsn: i64,
    pub current_configuration_quorum_progress: i64,
    pub catch_up_boundary: Option<i64>,
    pub catch_up_complete: bool,
    pub builds: Vec<BuildPostcondition>,
}

#[derive(Debug)]
struct RuntimeState {
    open: bool,
    role: ReplicaRole,
    write_status: AccessStatus,
    authority: Option<AdmittedAuthority>,
    replication_progress: Option<ReplicationProgress>,
    current_progress: i64,
    committed_lsn: i64,
    builds: BTreeMap<OperationId, BuildProgress>,
    outbound_builds: BTreeMap<OperationId, OutboundBuild>,
    local_writes: BTreeMap<OperationId, DurableLocalWrite>,
    effects: BTreeMap<u64, AppliedEffect>,
}

#[derive(Debug, Clone)]
struct AppliedEffect {
    effect: RuntimeEffect,
    result: RuntimeEffectResult,
}

#[derive(Debug, Clone)]
struct OutboundBuild {
    progress: BuildProgress,
    final_sequence: u64,
    next_sequence: u64,
    emitted: BTreeMap<u64, EmittedBuildItem>,
}

#[derive(Debug, Clone, Copy)]
struct EmittedBuildItem {
    lsn: i64,
    final_item: bool,
    snapshot_chunk: bool,
}

pub struct PendingWrite {
    pub lsn: i64,
    pub replication_items: Vec<proto::ReplicationItem>,
    pub build_items: Vec<proto::CopyItem>,
    completion: oneshot::Receiver<Result<i64>>,
}

impl PendingWrite {
    pub async fn committed(self) -> Result<WriteReceipt> {
        let committed_lsn = self
            .completion
            .await
            .map_err(|_| RuntimeError::WriteCompletionClosed)??;
        Ok(WriteReceipt {
            lsn: self.lsn,
            committed_lsn,
        })
    }
}

pub struct PodRuntime {
    identity: ReplicaIdentity,
    application: Arc<dyn StatefulApplication>,
    authority_store: Arc<dyn AuthorityStore>,
    state: RwLock<RuntimeState>,
    effect_lock: Mutex<()>,
    replicator: Mutex<Replicator>,
}

impl PodRuntime {
    pub fn new<A, S>(
        identity: ReplicaIdentity,
        application: Arc<A>,
        authority_store: Arc<S>,
    ) -> Self
    where
        A: StatefulApplication + 'static,
        S: AuthorityStore + 'static,
    {
        Self {
            replicator: Mutex::new(Replicator::new(identity.clone())),
            identity,
            application,
            authority_store,
            state: RwLock::new(RuntimeState {
                open: false,
                role: ReplicaRole::None,
                write_status: AccessStatus::NotPrimary,
                authority: None,
                replication_progress: None,
                current_progress: 0,
                committed_lsn: 0,
                builds: BTreeMap::new(),
                outbound_builds: BTreeMap::new(),
                local_writes: BTreeMap::new(),
                effects: BTreeMap::new(),
            }),
            effect_lock: Mutex::new(()),
        }
    }

    pub async fn serve<C>(&self, control_plane: &mut C) -> Result<()>
    where
        C: RuntimeControlPlane,
    {
        while let Some(effect) = control_plane.next_effect().await? {
            let result = self.apply_effect(effect).await?;
            control_plane.publish(result).await?;
        }
        Ok(())
    }

    pub async fn restore_authority(&self) -> Result<()> {
        let _guard = self.effect_lock.lock().await;
        let Some(authority) = self.authority_store.load().await? else {
            return Ok(());
        };
        authority.validate()?;
        if authority.local_identity != self.identity {
            return Err(RuntimeError::AuthorityMismatch(
                "persisted authority belongs to another runtime identity".to_string(),
            ));
        }
        let current_progress = self.state.read().await.current_progress;
        self.replicator
            .lock()
            .await
            .configure(authority.clone(), current_progress)?;
        let replication_progress = self
            .load_replication_progress_with_handoff(&authority)
            .await?;
        let mut state = self.state.write().await;
        state.authority = Some(authority);
        state.replication_progress = Some(replication_progress);
        Ok(())
    }

    pub async fn apply_effect(&self, effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        let _guard = self.effect_lock.lock().await;
        {
            let state = self.state.read().await;
            if let Some(previous) = state.effects.get(&effect.sequence) {
                if effect == previous.effect {
                    return Ok(previous.result.clone());
                }
                return Err(RuntimeError::EffectConflict {
                    sequence: effect.sequence,
                });
            }
            let expected = state
                .effects
                .last_key_value()
                .map_or(1, |(sequence, _)| sequence + 1);
            if effect.sequence != expected {
                return Err(RuntimeError::EffectOutOfOrder {
                    expected,
                    observed: effect.sequence,
                });
            }
        }

        self.execute_action(effect.action.clone()).await?;
        let result = RuntimeEffectResult {
            operation_id: effect.operation_id.clone(),
            sequence: effect.sequence,
            postcondition: self.postcondition().await,
        };
        self.state.write().await.effects.insert(
            result.sequence,
            AppliedEffect {
                effect,
                result: result.clone(),
            },
        );
        Ok(result)
    }

    pub async fn begin_write(&self, write: ClientWrite) -> Result<PendingWrite> {
        let _guard = self.effect_lock.lock().await;
        let state = self.state.read().await;
        if !state.open {
            return Err(RuntimeError::NotOpen);
        }
        if state.role != ReplicaRole::Primary {
            return Err(RuntimeError::NotPrimary);
        }
        if state.write_status != AccessStatus::Granted {
            return Err(RuntimeError::WriteClosed(state.write_status));
        }
        let authority = state
            .authority
            .as_ref()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if authority.primary_identity() != &self.identity {
            return Err(RuntimeError::AuthorityMismatch(
                "local runtime is not the admitted primary".to_string(),
            ));
        }
        if let Some(unresolved) = state
            .local_writes
            .values()
            .find(|pending| pending.operation_id != write.operation_id)
        {
            return Err(RuntimeError::LocalWritePending(
                unresolved.operation_id.to_string(),
            ));
        }
        drop(state);

        let durable_write = if let Some(existing) = self
            .authority_store
            .load_local_write(&write.operation_id)
            .await?
        {
            if existing.data != write.data {
                return Err(RuntimeError::Application(
                    "client operation ID was reused with different data".to_string(),
                ));
            }
            existing
        } else {
            let lsn = self.replicator.lock().await.reserve_write(&write)?;
            let reserved = DurableLocalWrite {
                operation_id: write.operation_id.clone(),
                lsn,
                data: write.data.clone(),
                phase: LocalWritePhase::Reserved,
            };
            self.authority_store.record_local_write(&reserved).await?;
            self.state
                .write()
                .await
                .local_writes
                .insert(write.operation_id.clone(), reserved.clone());
            reserved
        };
        let lsn = durable_write.lsn;
        let committed_lsn = self.replicator.lock().await.committed_lsn();
        let operation = Operation {
            lsn,
            committed_lsn,
            data: write.data.clone(),
        };
        if durable_write.phase == LocalWritePhase::Committed {
            self.replicator
                .lock()
                .await
                .restore_committed_write(&operation)?;
            self.state
                .write()
                .await
                .local_writes
                .remove(&write.operation_id);
            let (sender, completion) = oneshot::channel();
            let _ = sender.send(Ok(lsn));
            return Ok(PendingWrite {
                lsn,
                replication_items: Vec::new(),
                build_items: Vec::new(),
                completion,
            });
        }
        let progress = self.application.durable_progress().await?;
        let durable_ack = if progress.applied_lsn >= lsn {
            if !self.application.verify_applied(&operation).await? {
                return Err(RuntimeError::Application(
                    "reserved write conflicts with durable application state".to_string(),
                ));
            }
            progress
        } else {
            if progress.applied_lsn + 1 != lsn {
                return Err(RuntimeError::Application(format!(
                    "reserved LSN {lsn} is not contiguous with durable progress {}",
                    progress.applied_lsn
                )));
            }
            self.application.apply(operation.clone()).await?
        };
        validate_durable_ack(lsn, committed_lsn, durable_ack)?;
        let build_operation = operation.clone();
        let prior_phase = durable_write.phase;
        let registered = DurableLocalWrite {
            phase: LocalWritePhase::Registered,
            ..durable_write
        };
        self.authority_store.record_local_write(&registered).await?;
        self.state
            .write()
            .await
            .local_writes
            .insert(write.operation_id.clone(), registered);
        {
            let mut replicator = self.replicator.lock().await;
            if prior_phase == LocalWritePhase::Reserved {
                replicator.restore_write_reservation(&write, lsn)?;
            }
        }
        let PreparedWrite {
            lsn,
            items,
            completion,
        } = self
            .replicator
            .lock()
            .await
            .ensure_local_write_registered(&operation)?;
        self.finalize_ready_commit().await?;
        let mut state = self.state.write().await;
        state.current_progress = durable_ack.applied_lsn;
        let build_items = state
            .outbound_builds
            .values_mut()
            .filter(|build| build_operation.lsn > build.progress.authority.replication_boundary_lsn)
            .map(|build| {
                let sequence = build.progress.authority.snapshot_chunk_count
                    + 1
                    + (build_operation.lsn - build.progress.authority.replication_boundary_lsn)
                        as u64;
                build.next_sequence = build.next_sequence.max(sequence + 1);
                let item = proto::CopyItem {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    build_id: build.progress.authority.build_id.to_string(),
                    sender: Some(build.progress.authority.source.clone().into()),
                    receiver: Some(build.progress.authority.target.clone().into()),
                    epoch: Some(build.progress.authority.current_configuration.epoch.into()),
                    current_configuration_id: build
                        .progress
                        .authority
                        .current_configuration
                        .configuration_id
                        .to_string(),
                    sequence,
                    lsn: build_operation.lsn,
                    committed_lsn: build_operation.committed_lsn.min(build_operation.lsn),
                    replication_boundary_lsn: build.progress.authority.replication_boundary_lsn,
                    final_item: false,
                    data: build_operation.data.to_vec(),
                    snapshot_chunk: false,
                };
                build.emitted.insert(
                    sequence,
                    EmittedBuildItem {
                        lsn: build_operation.lsn,
                        final_item: false,
                        snapshot_chunk: false,
                    },
                );
                item
            })
            .collect();
        drop(state);
        Ok(PendingWrite {
            lsn,
            replication_items: items,
            build_items,
            completion,
        })
    }

    pub async fn accept_acknowledgement(
        &self,
        acknowledgement: proto::ReplicationAck,
    ) -> Result<()> {
        let _guard = self.effect_lock.lock().await;
        let acknowledgement = normalize_replication_ack(acknowledgement)
            .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?;
        self.replicator.lock().await.acknowledge(&acknowledgement)?;
        self.finalize_ready_commit().await
    }

    pub async fn prepare_copy(&self, request: PrepareCopyRequest) -> Result<PreparedCopy> {
        let _guard = self.effect_lock.lock().await;
        let state = self.state.read().await;
        if !state.open {
            return Err(RuntimeError::NotOpen);
        }
        if state.role != ReplicaRole::Primary {
            return Err(RuntimeError::NotPrimary);
        }
        let (kind, configuration) = match request.configuration.clone() {
            BuildConfiguration::Current => {
                let authority = state
                    .authority
                    .clone()
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?;
                if authority.primary_identity() != &self.identity {
                    return Err(RuntimeError::NotPrimary);
                }
                (
                    BuildAuthorityKind::Provisioning,
                    authority.current_configuration,
                )
            }
            BuildConfiguration::Bootstrap(configuration) => {
                if state.write_status == AccessStatus::Granted {
                    return Err(RuntimeError::WriteClosed(AccessStatus::Granted));
                }
                (BuildAuthorityKind::Bootstrap, configuration)
            }
        };
        let local_committed_lsn = state.committed_lsn;
        drop(state);

        let replicator = self.replicator.lock().await;
        let replicator_progress = replicator.highest_lsn();
        let committed_lsn = local_committed_lsn.max(replicator.committed_lsn());
        let retained = replicator.retained_operations_from(1);
        drop(replicator);
        let application_progress = self.application.durable_progress().await?;
        let current_highest = replicator_progress.max(application_progress.applied_lsn);
        let existing = self.authority_store.load_build(&request.build_id).await?;
        let boundary = existing.as_ref().map_or(current_highest, |authority| {
            authority.replication_boundary_lsn
        });
        if current_highest < boundary {
            return Err(RuntimeError::Application(
                "application progress regressed below the copy boundary".to_string(),
            ));
        }
        let mut copy_stream = self
            .application
            .get_copy_state(boundary, request.copy_context)
            .await?;
        let mut chunks = Vec::new();
        while let Some(chunk) = copy_stream.next().await {
            chunks.push(chunk?);
        }
        let candidate = BuildAuthority {
            build_id: request.build_id.clone(),
            kind,
            source: self.identity.clone(),
            target: request.target,
            current_configuration: configuration,
            replication_boundary_lsn: boundary,
            snapshot_chunk_count: chunks.len() as u64,
        };
        candidate.validate()?;
        let build_authority = if let Some(existing) = existing {
            if existing != candidate {
                return Err(RuntimeError::AuthorityMismatch(
                    "build ID is already bound to different immutable authority".to_string(),
                ));
            }
            existing
        } else {
            self.authority_store.admit_build(&candidate).await?;
            candidate
        };
        let build_progress = self
            .authority_store
            .load_build_progress(&build_authority.build_id)
            .await?
            .unwrap_or(DurableBuildProgress {
                authority: build_authority.clone(),
                last_sequence: 0,
                durable_lsn: 0,
                completed: false,
            });
        if build_progress.authority != build_authority {
            return Err(RuntimeError::AuthorityMismatch(
                "build progress belongs to different authority".to_string(),
            ));
        }

        let mut operations = BTreeMap::new();
        let mut stream = self
            .application
            .get_replication_operations(boundary + 1, current_highest)
            .await?;
        while let Some(operation) = stream.next().await {
            let operation = operation?;
            if operation.lsn > boundary && operation.lsn <= current_highest {
                insert_copy_operation(&mut operations, operation)?;
            }
        }
        for operation in retained
            .into_iter()
            .filter(|operation| operation.lsn > boundary && operation.lsn <= current_highest)
        {
            insert_copy_operation(&mut operations, operation)?;
        }
        if current_highest > boundary
            && ((boundary + 1)..=current_highest).any(|lsn| !operations.contains_key(&lsn))
        {
            return Err(RuntimeError::InvalidReplication(
                "retained operations do not close the post-snapshot gap".to_string(),
            ));
        }

        let mut items = Vec::with_capacity(chunks.len() + operations.len() + 1);
        for (index, chunk) in chunks.into_iter().enumerate() {
            items.push(proto::CopyItem {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                build_id: build_authority.build_id.to_string(),
                sender: Some(build_authority.source.clone().into()),
                receiver: Some(build_authority.target.clone().into()),
                epoch: Some(build_authority.current_configuration.epoch.into()),
                current_configuration_id: build_authority
                    .current_configuration
                    .configuration_id
                    .to_string(),
                sequence: index as u64 + 1,
                lsn: 0,
                committed_lsn: 0,
                replication_boundary_lsn: boundary,
                final_item: false,
                data: chunk.data.to_vec(),
                snapshot_chunk: true,
            });
        }
        let final_sequence = build_authority.snapshot_chunk_count + 1;
        items.push(proto::CopyItem {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            build_id: build_authority.build_id.to_string(),
            sender: Some(build_authority.source.clone().into()),
            receiver: Some(build_authority.target.clone().into()),
            epoch: Some(build_authority.current_configuration.epoch.into()),
            current_configuration_id: build_authority
                .current_configuration
                .configuration_id
                .to_string(),
            sequence: final_sequence,
            lsn: boundary,
            committed_lsn: committed_lsn.min(boundary),
            replication_boundary_lsn: boundary,
            final_item: true,
            data: Vec::new(),
            snapshot_chunk: false,
        });
        for operation in operations.into_values() {
            items.push(proto::CopyItem {
                protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                build_id: build_authority.build_id.to_string(),
                sender: Some(build_authority.source.clone().into()),
                receiver: Some(build_authority.target.clone().into()),
                epoch: Some(build_authority.current_configuration.epoch.into()),
                current_configuration_id: build_authority
                    .current_configuration
                    .configuration_id
                    .to_string(),
                sequence: final_sequence + (operation.lsn - boundary) as u64,
                lsn: operation.lsn,
                committed_lsn: operation.committed_lsn.min(operation.lsn),
                replication_boundary_lsn: boundary,
                final_item: false,
                data: operation.data.to_vec(),
                snapshot_chunk: false,
            });
        }
        items.sort_by_key(|item| item.sequence);
        let emitted = items
            .iter()
            .map(|item| {
                (
                    item.sequence,
                    EmittedBuildItem {
                        lsn: item.lsn,
                        final_item: item.final_item,
                        snapshot_chunk: item.snapshot_chunk,
                    },
                )
            })
            .collect();
        self.state.write().await.outbound_builds.insert(
            build_authority.build_id.clone(),
            OutboundBuild {
                progress: build_progress,
                final_sequence,
                next_sequence: final_sequence + (current_highest - boundary) as u64 + 1,
                emitted,
            },
        );
        Ok(PreparedCopy {
            authority: build_authority,
            items,
        })
    }

    pub async fn accept_copy_acknowledgement(&self, ack: proto::CopyAck) -> Result<()> {
        let _guard = self.effect_lock.lock().await;
        let acknowledgement = normalize_copy_ack(ack)
            .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?;
        let mut state = self.state.write().await;
        let build = state
            .outbound_builds
            .get_mut(&acknowledgement.build_id)
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let authority = &build.progress.authority;
        if acknowledgement.sender != authority.source
            || acknowledgement.receiver != authority.target
            || acknowledgement.epoch != authority.current_configuration.epoch
            || acknowledgement.current_configuration_id
                != authority.current_configuration.configuration_id
            || acknowledgement.replication_boundary_lsn != authority.replication_boundary_lsn
        {
            return Err(RuntimeError::AuthorityMismatch(
                "copy acknowledgement does not match the active build".to_string(),
            ));
        }
        let emitted = build
            .emitted
            .get(&acknowledgement.sequence)
            .ok_or_else(|| {
                RuntimeError::InvalidReplication(
                    "copy acknowledgement does not match an emitted item".to_string(),
                )
            })?;
        if acknowledgement.final_item != emitted.final_item
            || acknowledgement.snapshot_chunk != emitted.snapshot_chunk
            || acknowledgement.final_item != (acknowledgement.sequence == build.final_sequence)
        {
            return Err(RuntimeError::InvalidReplication(
                "copy acknowledgement does not match an emitted item".to_string(),
            ));
        }
        let max_emitted_lsn = build
            .emitted
            .values()
            .map(|item| item.lsn)
            .max()
            .unwrap_or(0);
        if (!acknowledgement.final_item && acknowledgement.durable_lsn < emitted.lsn)
            || acknowledgement.durable_lsn > max_emitted_lsn
        {
            return Err(RuntimeError::InvalidReplication(
                "copy acknowledgement exceeds emitted durable progress".to_string(),
            ));
        }
        build.progress.last_sequence = build.progress.last_sequence.max(acknowledgement.sequence);
        build.progress.durable_lsn = build.progress.durable_lsn.max(acknowledgement.durable_lsn);
        if acknowledgement.final_item {
            build.progress.completed = true;
        }
        let progress = build.progress.clone();
        drop(state);
        self.authority_store
            .record_build_progress(&progress)
            .await?;
        Ok(())
    }

    pub async fn receive_copy_item(&self, item: proto::CopyItem) -> Result<proto::CopyAck> {
        let _guard = self.effect_lock.lock().await;
        let envelope = normalize_copy_item(item)
            .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?;
        let state = self.state.read().await;
        if !state.open {
            return Err(RuntimeError::NotOpen);
        }
        if state.role != ReplicaRole::IdleSecondary {
            return Err(RuntimeError::AuthorityMismatch(
                "copy target must be an Idle Secondary".to_string(),
            ));
        }
        let progress = state
            .builds
            .get(&envelope.build_id)
            .cloned()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        drop(state);
        let authority = self
            .authority_store
            .load_build(&envelope.build_id)
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if authority != progress.authority {
            return Err(RuntimeError::AuthorityMismatch(
                "durable build authority differs from runtime authority".to_string(),
            ));
        }
        authority.validate()?;
        authority.validate_envelope(&envelope)?;
        let final_sequence = authority.snapshot_chunk_count + 1;
        let expected_snapshot_chunk = envelope.sequence <= authority.snapshot_chunk_count;
        let expected_final = envelope.sequence == final_sequence;
        if envelope.snapshot_chunk != expected_snapshot_chunk
            || envelope.final_item != expected_final
        {
            return Err(RuntimeError::InvalidReplication(
                "copy item kind does not match its authority-bound sequence".to_string(),
            ));
        }

        let durable_lsn = if envelope.sequence <= progress.last_sequence {
            if envelope.snapshot_chunk {
                self.application
                    .apply_copy_chunk(
                        &envelope.build_id,
                        envelope.sequence,
                        crate::application::CopyChunk {
                            data: Bytes::from(envelope.data.clone()),
                        },
                    )
                    .await?;
                0
            } else if envelope.final_item {
                if !progress.completed {
                    return Err(RuntimeError::InvalidReplication(
                        "duplicate final copy marker preceded completion".to_string(),
                    ));
                }
                envelope.replication_boundary_lsn
            } else {
                let operation = Operation {
                    lsn: envelope.lsn,
                    committed_lsn: envelope.committed_lsn,
                    data: Bytes::from(envelope.data.clone()),
                };
                if !self.application.verify_applied(&operation).await? {
                    return Err(RuntimeError::InvalidReplication(
                        "duplicate copy item has conflicting durable contents".to_string(),
                    ));
                }
                progress.durable_lsn
            }
        } else {
            if envelope.sequence != progress.last_sequence + 1 {
                return Err(RuntimeError::InvalidReplication(format!(
                    "copy sequence gap: expected {}, observed {}",
                    progress.last_sequence + 1,
                    envelope.sequence
                )));
            }
            if envelope.snapshot_chunk {
                self.application
                    .apply_copy_chunk(
                        &envelope.build_id,
                        envelope.sequence,
                        crate::application::CopyChunk {
                            data: Bytes::from(envelope.data.clone()),
                        },
                    )
                    .await?;
                let updated = DurableBuildProgress {
                    authority: progress.authority.clone(),
                    last_sequence: envelope.sequence,
                    durable_lsn: progress.durable_lsn,
                    completed: false,
                };
                self.authority_store.record_build_progress(&updated).await?;
                self.state
                    .write()
                    .await
                    .builds
                    .insert(envelope.build_id.clone(), updated);
                progress.durable_lsn
            } else if envelope.final_item {
                let durable = self
                    .application
                    .finish_copy(
                        &envelope.build_id,
                        envelope.replication_boundary_lsn,
                        envelope.committed_lsn,
                    )
                    .await?;
                if durable.applied_lsn < envelope.replication_boundary_lsn {
                    return Err(RuntimeError::Application(
                        "application lost the durable copy boundary".to_string(),
                    ));
                }
                let updated = DurableBuildProgress {
                    authority: progress.authority.clone(),
                    last_sequence: envelope.sequence,
                    durable_lsn: envelope.replication_boundary_lsn,
                    completed: true,
                };
                self.authority_store.record_build_progress(&updated).await?;
                let mut state = self.state.write().await;
                state.builds.insert(envelope.build_id.clone(), updated);
                state.current_progress = state.current_progress.max(durable.applied_lsn);
                state.committed_lsn = state.committed_lsn.max(durable.committed_lsn);
                envelope.replication_boundary_lsn
            } else {
                if !progress.completed {
                    return Err(RuntimeError::InvalidReplication(
                        "live build replication arrived before snapshot completion".to_string(),
                    ));
                }
                if envelope.lsn != progress.durable_lsn + 1 {
                    return Err(RuntimeError::InvalidReplication(format!(
                        "copy LSN gap: expected {}, observed {}",
                        progress.durable_lsn + 1,
                        envelope.lsn
                    )));
                }
                let operation = Operation {
                    lsn: envelope.lsn,
                    committed_lsn: envelope.committed_lsn,
                    data: Bytes::from(envelope.data.clone()),
                };
                let application_progress = self.application.durable_progress().await?;
                let durable = if application_progress.applied_lsn >= envelope.lsn {
                    if !self.application.verify_applied(&operation).await? {
                        return Err(RuntimeError::InvalidReplication(
                            "copy item conflicts with durable application state".to_string(),
                        ));
                    }
                    application_progress
                } else {
                    if application_progress.applied_lsn + 1 != envelope.lsn {
                        return Err(RuntimeError::InvalidReplication(format!(
                            "copy application gap: expected LSN {}, observed {}",
                            application_progress.applied_lsn + 1,
                            envelope.lsn
                        )));
                    }
                    let durable = self.application.apply(operation).await?;
                    validate_durable_ack(envelope.lsn, envelope.committed_lsn, durable)?;
                    durable
                };
                let updated = DurableBuildProgress {
                    authority: progress.authority.clone(),
                    last_sequence: envelope.sequence,
                    durable_lsn: envelope.lsn,
                    completed: progress.completed,
                };
                self.authority_store.record_build_progress(&updated).await?;
                let mut state = self.state.write().await;
                state.builds.insert(envelope.build_id.clone(), updated);
                state.current_progress = state.current_progress.max(durable.applied_lsn);
                state.committed_lsn = state.committed_lsn.max(durable.committed_lsn);
                envelope.lsn
            }
        };
        Ok(proto::CopyAck {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            build_id: envelope.build_id.to_string(),
            sender: Some(envelope.sender.into()),
            receiver: Some(envelope.receiver.into()),
            epoch: Some(envelope.epoch.into()),
            current_configuration_id: envelope.current_configuration_id.to_string(),
            sequence: envelope.sequence,
            durable_lsn,
            replication_boundary_lsn: envelope.replication_boundary_lsn,
            final_item: envelope.final_item,
            snapshot_chunk: envelope.snapshot_chunk,
        })
    }

    pub async fn receive_replication(
        &self,
        item: proto::ReplicationItem,
    ) -> Result<proto::ReplicationAck> {
        let _guard = self.effect_lock.lock().await;
        let envelope = normalize_replication_item(item)
            .map_err(|error| RuntimeError::InvalidReplication(error.to_string()))?;
        let state = self.state.read().await;
        if !state.open {
            return Err(RuntimeError::NotOpen);
        }
        let in_memory_authority = state
            .authority
            .clone()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let mut replication_progress = state
            .replication_progress
            .clone()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        let role = state.role;
        drop(state);
        let authority = self
            .authority_store
            .load()
            .await?
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        authority.validate()?;
        if authority != in_memory_authority {
            return Err(RuntimeError::AuthorityMismatch(
                "durable authority differs from runtime authority".to_string(),
            ));
        }
        if authority.local_identity != self.identity || envelope.receiver != self.identity {
            return Err(RuntimeError::AuthorityMismatch(
                "replication target differs from the local durable identity".to_string(),
            ));
        }
        if role != authority.local_role()
            || !matches!(
                role,
                ReplicaRole::ActiveSecondary | ReplicaRole::IdleSecondary
            )
        {
            return Err(RuntimeError::AuthorityMismatch(
                "runtime role is not admitted to receive replication".to_string(),
            ));
        }
        authority.validate_envelope(&envelope)?;
        if replication_progress.fence != authority.fence() {
            return Err(RuntimeError::AuthorityMismatch(
                "replication progress belongs to another authority".to_string(),
            ));
        }
        if envelope.lsn > replication_progress.verified_lsn + 1 {
            return Err(RuntimeError::InvalidReplication(format!(
                "authority verification gap: expected at most LSN {}, observed {}",
                replication_progress.verified_lsn + 1,
                envelope.lsn
            )));
        }
        let operation = Operation {
            lsn: envelope.lsn,
            committed_lsn: envelope.committed_lsn,
            data: Bytes::from(envelope.data.clone()),
        };
        let application_progress = self.application.durable_progress().await?;
        let durable_ack = if envelope.lsn <= application_progress.applied_lsn {
            if !self.application.verify_applied(&operation).await? {
                return Err(RuntimeError::InvalidReplication(
                    "authority operation conflicts with durable application state".to_string(),
                ));
            }
            if envelope.committed_lsn > application_progress.committed_lsn {
                self.application.commit(envelope.committed_lsn).await?
            } else {
                application_progress
            }
        } else {
            if envelope.lsn != application_progress.applied_lsn + 1 {
                return Err(RuntimeError::InvalidReplication(format!(
                    "application gap: expected LSN {}, observed {}",
                    application_progress.applied_lsn + 1,
                    envelope.lsn
                )));
            }
            let durable_ack = self.application.apply(operation).await?;
            validate_durable_ack(envelope.lsn, envelope.committed_lsn, durable_ack)?;
            durable_ack
        };
        if envelope.lsn == replication_progress.verified_lsn + 1 {
            replication_progress.verified_lsn = envelope.lsn;
            self.authority_store
                .record_replication_progress(&replication_progress)
                .await?;
        }
        let mut state = self.state.write().await;
        state.replication_progress = Some(replication_progress.clone());
        state.current_progress = durable_ack.applied_lsn;
        state.committed_lsn = state.committed_lsn.max(durable_ack.committed_lsn);
        drop(state);
        let acknowledged_committed_lsn = durable_ack
            .committed_lsn
            .min(replication_progress.verified_lsn);
        Ok(proto::ReplicationAck {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            sender: Some(envelope.sender.into()),
            receiver: Some(self.identity.clone().into()),
            epoch: Some(envelope.epoch.into()),
            previous_configuration_id: envelope
                .previous_configuration_id
                .map_or_else(String::new, |configuration_id| configuration_id.to_string()),
            current_configuration_id: envelope.current_configuration_id.to_string(),
            received_lsn: envelope.lsn,
            applied_lsn: replication_progress.verified_lsn,
            committed_lsn: acknowledged_committed_lsn,
        })
    }

    pub async fn snapshot(&self) -> RuntimeSnapshot {
        let _guard = self.effect_lock.lock().await;
        let state = self.state.read().await;
        let snapshot = (
            state.open,
            state.role,
            state.write_status,
            state.authority.clone(),
            state.current_progress,
            state
                .replication_progress
                .as_ref()
                .map(|progress| progress.verified_lsn),
            state.committed_lsn,
            build_postconditions(&state),
        );
        drop(state);
        let replicator = self.replicator.lock().await;
        RuntimeSnapshot {
            identity: self.identity.clone(),
            open: snapshot.0,
            role: snapshot.1,
            write_status: snapshot.2,
            authority: snapshot.3,
            current_progress: snapshot.4,
            verified_replication_lsn: snapshot.5,
            committed_lsn: snapshot.6.max(replicator.committed_lsn()),
            current_configuration_quorum_progress: replicator
                .current_configuration_quorum_progress(),
            catch_up_boundary: replicator.catch_up_boundary(),
            catch_up_complete: replicator.catch_up_complete(),
            builds: snapshot.7,
        }
    }

    async fn execute_action(&self, action: RuntimeEffectAction) -> Result<()> {
        match action {
            RuntimeEffectAction::Open(mode) => {
                self.application
                    .open(OpenContext {
                        identity: self.identity.clone(),
                        mode,
                    })
                    .await?;
                let progress = self.application.durable_progress().await?;
                let local_writes = self.authority_store.load_local_writes().await?;
                let mut state = self.state.write().await;
                state.open = true;
                state.current_progress = progress.applied_lsn;
                state.committed_lsn = progress.committed_lsn;
                state.local_writes = local_writes
                    .into_iter()
                    .map(|write| (write.operation_id.clone(), write))
                    .collect();
            }
            RuntimeEffectAction::AdmitAuthority(authority) => {
                let authority = *authority;
                authority.validate()?;
                if authority.local_identity != self.identity {
                    return Err(RuntimeError::AuthorityMismatch(
                        "authority target differs from runtime identity".to_string(),
                    ));
                }
                self.authority_store.admit(&authority).await?;
                let current_progress = self.state.read().await.current_progress;
                self.replicator
                    .lock()
                    .await
                    .configure(authority.clone(), current_progress)?;
                let replication_progress = self
                    .load_replication_progress_with_handoff(&authority)
                    .await?;
                let mut state = self.state.write().await;
                state.authority = Some(authority);
                state.replication_progress = Some(replication_progress);
            }
            RuntimeEffectAction::AdmitBuildAuthority(authority) => {
                let authority = *authority;
                authority.validate()?;
                if authority.target != self.identity && authority.source != self.identity {
                    return Err(RuntimeError::AuthorityMismatch(
                        "build authority does not address this runtime".to_string(),
                    ));
                }
                if let Some(existing) = self.authority_store.load_build(&authority.build_id).await?
                {
                    if existing != authority {
                        return Err(RuntimeError::AuthorityMismatch(
                            "build ID is already bound to different authority".to_string(),
                        ));
                    }
                } else {
                    self.authority_store.admit_build(&authority).await?;
                }
                let progress = self
                    .authority_store
                    .load_build_progress(&authority.build_id)
                    .await?
                    .unwrap_or(DurableBuildProgress {
                        authority: authority.clone(),
                        last_sequence: 0,
                        durable_lsn: 0,
                        completed: false,
                    });
                if progress.authority != authority {
                    return Err(RuntimeError::AuthorityMismatch(
                        "build progress belongs to different authority".to_string(),
                    ));
                }
                if authority.target == self.identity {
                    self.state
                        .write()
                        .await
                        .builds
                        .insert(authority.build_id.clone(), progress);
                }
            }
            RuntimeEffectAction::ChangeRole(role) => {
                if !self.state.read().await.open {
                    return Err(RuntimeError::NotOpen);
                }
                self.state.write().await.write_status = AccessStatus::ReconfigurationPending;
                if role != ReplicaRole::Primary {
                    self.replicator.lock().await.fence_client_writes();
                }
                let _ = self.application.change_role(role).await?;
                self.state.write().await.role = role;
            }
            RuntimeEffectAction::SetWriteStatus(write_status) => {
                let state = self.state.read().await;
                if write_status == AccessStatus::Granted {
                    if state.role != ReplicaRole::Primary {
                        return Err(RuntimeError::NotPrimary);
                    }
                    let authority = state
                        .authority
                        .as_ref()
                        .ok_or(RuntimeError::AuthorityNotAdmitted)?;
                    if authority.primary_identity() != &self.identity {
                        return Err(RuntimeError::AuthorityMismatch(
                            "write grant target is not the admitted primary".to_string(),
                        ));
                    }
                }
                drop(state);
                if write_status != AccessStatus::Granted {
                    self.replicator.lock().await.fence_client_writes();
                }
                self.state.write().await.write_status = write_status;
            }
            RuntimeEffectAction::RefreshApplicationProgress => {
                let progress = self.application.durable_progress().await?;
                {
                    let mut state = self.state.write().await;
                    state.current_progress = state.current_progress.max(progress.applied_lsn);
                    state.committed_lsn = state.committed_lsn.max(progress.committed_lsn);
                }
                if self.state.read().await.authority.is_some() {
                    self.replicator
                        .lock()
                        .await
                        .record_local_progress(progress.applied_lsn)?;
                    self.finalize_ready_commit().await?;
                }
            }
            RuntimeEffectAction::RetireBuild(build_id) => {
                let mut state = self.state.write().await;
                state.builds.remove(&build_id);
                state.outbound_builds.remove(&build_id);
            }
            RuntimeEffectAction::Close => {
                {
                    let mut state = self.state.write().await;
                    state.write_status = AccessStatus::ReconfigurationPending;
                    state.open = false;
                }
                self.replicator.lock().await.fence_client_writes();
                self.application.close().await?;
                let mut state = self.state.write().await;
                state.role = ReplicaRole::None;
                state.write_status = AccessStatus::NotPrimary;
            }
            RuntimeEffectAction::Abort => {
                {
                    let mut state = self.state.write().await;
                    state.open = false;
                    state.role = ReplicaRole::None;
                    state.write_status = AccessStatus::NotPrimary;
                }
                self.replicator.lock().await.fence_client_writes();
                self.application.abort();
            }
        }
        Ok(())
    }

    async fn postcondition(&self) -> RuntimePostcondition {
        let state = self.state.read().await;
        let postcondition = (
            state.open,
            state.role,
            state.write_status,
            state.authority.clone(),
            state.current_progress,
            state
                .replication_progress
                .as_ref()
                .map(|progress| progress.verified_lsn),
            state.committed_lsn,
            build_postconditions(&state),
        );
        drop(state);
        let replicator = self.replicator.lock().await;
        RuntimePostcondition {
            open: postcondition.0,
            role: postcondition.1,
            write_status: postcondition.2,
            authority: postcondition.3,
            current_progress: postcondition.4,
            verified_replication_lsn: postcondition.5,
            committed_lsn: postcondition.6.max(replicator.committed_lsn()),
            current_configuration_quorum_progress: replicator
                .current_configuration_quorum_progress(),
            catch_up_boundary: replicator.catch_up_boundary(),
            catch_up_complete: replicator.catch_up_complete(),
            builds: postcondition.7,
        }
    }

    async fn finalize_ready_commit(&self) -> Result<()> {
        let Some(ready_lsn) = self.replicator.lock().await.ready_commit_lsn() else {
            return Ok(());
        };
        let progress = self.application.commit(ready_lsn).await?;
        if progress.committed_lsn < ready_lsn || progress.applied_lsn < progress.committed_lsn {
            return Err(RuntimeError::Application(
                "application did not durably record quorum-ready progress".to_string(),
            ));
        }
        let committed_writes = self
            .state
            .read()
            .await
            .local_writes
            .values()
            .filter(|write| write.phase == LocalWritePhase::Registered && write.lsn <= ready_lsn)
            .cloned()
            .map(|write| DurableLocalWrite {
                phase: LocalWritePhase::Committed,
                ..write
            })
            .collect::<Vec<_>>();
        for write in &committed_writes {
            self.authority_store.record_local_write(write).await?;
        }
        self.replicator.lock().await.finalize_commit(ready_lsn)?;
        let mut state = self.state.write().await;
        for write in committed_writes {
            state.local_writes.remove(&write.operation_id);
        }
        state.committed_lsn = state.committed_lsn.max(progress.committed_lsn);
        Ok(())
    }

    async fn load_replication_progress_with_handoff(
        &self,
        authority: &AdmittedAuthority,
    ) -> Result<ReplicationProgress> {
        let mut progress = self
            .authority_store
            .load_replication_progress(&authority.fence())
            .await?
            .unwrap_or(ReplicationProgress {
                fence: authority.fence(),
                verified_lsn: 0,
            });
        if authority.previous_configuration.is_none()
            && let Some(configuration_progress) = self
                .authority_store
                .load_configuration_progress(
                    authority.current_configuration.epoch,
                    &authority.current_configuration.configuration_id,
                )
                .await?
        {
            progress.verified_lsn = progress
                .verified_lsn
                .max(configuration_progress.verified_lsn);
        }
        if let Some(previous) = authority.previous_configuration.as_ref()
            && previous.primary_id == authority.current_configuration.primary_id
            && previous
                .members
                .iter()
                .find(|member| member.identity.replica_id == previous.primary_id)
                .zip(
                    authority
                        .current_configuration
                        .members
                        .iter()
                        .find(|member| {
                            member.identity.replica_id == authority.current_configuration.primary_id
                        }),
                )
                .is_some_and(|(previous, current)| previous.identity == current.identity)
            && previous
                .members
                .iter()
                .any(|member| member.identity == self.identity)
            && authority
                .current_configuration
                .members
                .iter()
                .any(|member| member.identity == self.identity)
        {
            let previous_fence = crate::authority::AuthorityFence {
                epoch: previous.epoch,
                previous_configuration_id: None,
                current_configuration_id: previous.configuration_id.clone(),
            };
            if let Some(previous_progress) = self
                .authority_store
                .load_replication_progress(&previous_fence)
                .await?
            {
                progress.verified_lsn = progress.verified_lsn.max(previous_progress.verified_lsn);
            }
        }
        let handoff_lsn = self
            .state
            .read()
            .await
            .builds
            .values()
            .filter(|build| build.completed)
            .filter(|build| build.authority.target == self.identity)
            .filter(|build| build_handoff_matches(&build.authority, authority))
            .map(|build| build.durable_lsn)
            .max();
        if let Some(handoff_lsn) = handoff_lsn
            && handoff_lsn > progress.verified_lsn
        {
            progress.verified_lsn = handoff_lsn;
        }
        self.authority_store
            .record_replication_progress(&progress)
            .await?;
        Ok(progress)
    }
}

fn validate_durable_ack(
    lsn: i64,
    required_committed_lsn: i64,
    acknowledgement: DurableApplicationAck,
) -> Result<()> {
    if acknowledgement.applied_lsn != lsn
        || acknowledgement.committed_lsn < required_committed_lsn
        || acknowledgement.committed_lsn > acknowledgement.applied_lsn
    {
        return Err(RuntimeError::Application(
            "durable acknowledgement does not prove application acceptance".to_string(),
        ));
    }
    Ok(())
}

fn insert_copy_operation(
    operations: &mut BTreeMap<i64, Operation>,
    operation: Operation,
) -> Result<()> {
    if operation.lsn <= 0 {
        return Err(RuntimeError::InvalidReplication(
            "copy operation LSN must be positive".to_string(),
        ));
    }

    if let Some(existing) = operations.get_mut(&operation.lsn) {
        if existing.data != operation.data {
            return Err(RuntimeError::InvalidReplication(
                "copy and retained streams disagree at the same LSN".to_string(),
            ));
        }
        existing.committed_lsn = existing.committed_lsn.max(operation.committed_lsn);
        return Ok(());
    }
    operations.insert(operation.lsn, operation);
    Ok(())
}

fn build_handoff_matches(build: &BuildAuthority, authority: &AdmittedAuthority) -> bool {
    if !authority
        .current_configuration
        .members
        .iter()
        .any(|member| member.identity == build.target)
        || authority.primary_identity() != &build.source
    {
        return false;
    }
    match build.kind {
        BuildAuthorityKind::Bootstrap => {
            authority.transition_kind == Some(kuberic_protocol::types::TransitionKind::Bootstrap)
                && authority.previous_configuration.is_none()
                && authority.current_configuration.configuration_id
                    == build.current_configuration.configuration_id
        }
        BuildAuthorityKind::Provisioning => {
            authority
                .previous_configuration
                .as_ref()
                .is_some_and(|previous| {
                    previous.configuration_id == build.current_configuration.configuration_id
                })
        }
    }
}

impl From<BuildProgress> for BuildPostcondition {
    fn from(value: BuildProgress) -> Self {
        Self {
            authority: value.authority,
            last_sequence: value.last_sequence,
            durable_lsn: value.durable_lsn,
            completed: value.completed,
        }
    }
}

fn build_postconditions(state: &RuntimeState) -> Vec<BuildPostcondition> {
    state
        .builds
        .values()
        .cloned()
        .chain(
            state
                .outbound_builds
                .values()
                .map(|build| build.progress.clone()),
        )
        .map(BuildPostcondition::from)
        .collect()
}
