use std::collections::BTreeMap;
use std::sync::Arc;

use bytes::Bytes;
use futures::StreamExt;
use kuberic_protocol::types::{AccessStatus, OperationId, ReplicaIdentity, ReplicaRole};
use kuberic_wire::{
    normalize_copy_ack, normalize_copy_item, normalize_replication_ack, normalize_replication_item,
    proto,
};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, RwLock, oneshot};

use crate::application::{
    ClientWrite, DurableApplicationAck, Operation, StatefulApplication, WriteReceipt,
};
use crate::authority::{AdmittedAuthority, AuthorityStore, BuildAuthority};
use crate::effects::{
    BuildPostcondition, RuntimeControlPlane, RuntimeEffect, RuntimeEffectAction,
    RuntimeEffectResult, RuntimePostcondition,
};
use crate::replicator::copy::{BuildProgress, PreparedCopy};
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
    current_progress: i64,
    committed_lsn: i64,
    received_digests: BTreeMap<i64, [u8; 32]>,
    builds: BTreeMap<OperationId, BuildProgress>,
    outbound_builds: BTreeMap<OperationId, OutboundBuild>,
    effect_results: BTreeMap<u64, RuntimeEffectResult>,
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
                current_progress: 0,
                committed_lsn: 0,
                received_digests: BTreeMap::new(),
                builds: BTreeMap::new(),
                outbound_builds: BTreeMap::new(),
                effect_results: BTreeMap::new(),
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
        self.state.write().await.authority = Some(authority);
        Ok(())
    }

    pub async fn apply_effect(&self, effect: RuntimeEffect) -> Result<RuntimeEffectResult> {
        let _guard = self.effect_lock.lock().await;
        {
            let state = self.state.read().await;
            if let Some(previous) = state.effect_results.get(&effect.sequence) {
                if effect.operation_id == previous.operation_id {
                    return Ok(previous.clone());
                }
                return Err(RuntimeError::EffectConflict {
                    sequence: effect.sequence,
                });
            }
            let expected = state
                .effect_results
                .last_key_value()
                .map_or(1, |(sequence, _)| sequence + 1);
            if effect.sequence != expected {
                return Err(RuntimeError::EffectOutOfOrder {
                    expected,
                    observed: effect.sequence,
                });
            }
        }

        self.execute_action(effect.action).await?;
        let result = RuntimeEffectResult {
            operation_id: effect.operation_id,
            sequence: effect.sequence,
            postcondition: self.postcondition().await,
        };
        self.state
            .write()
            .await
            .effect_results
            .insert(result.sequence, result.clone());
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
        drop(state);

        let lsn = self.replicator.lock().await.reserve_lsn()?;
        let committed_lsn = self.replicator.lock().await.committed_lsn();
        let operation = Operation {
            lsn,
            committed_lsn,
            data: write.data,
        };
        let durable_ack = self.application.apply(operation.clone()).await?;
        let build_operation = operation.clone();
        let PreparedWrite {
            lsn,
            items,
            completion,
        } = self
            .replicator
            .lock()
            .await
            .record_local_write(operation, durable_ack)?;
        self.finalize_ready_commit().await?;
        let mut state = self.state.write().await;
        state.current_progress = lsn;
        let build_items = state
            .outbound_builds
            .values_mut()
            .map(|build| {
                let sequence = build.next_sequence;
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
                };
                build.next_sequence += 1;
                build.emitted.insert(
                    sequence,
                    EmittedBuildItem {
                        lsn: build_operation.lsn,
                        final_item: false,
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

    pub async fn prepare_copy(
        &self,
        build_id: OperationId,
        target: ReplicaIdentity,
    ) -> Result<PreparedCopy> {
        let _guard = self.effect_lock.lock().await;
        let state = self.state.read().await;
        if !state.open {
            return Err(RuntimeError::NotOpen);
        }
        let authority = state
            .authority
            .clone()
            .ok_or(RuntimeError::AuthorityNotAdmitted)?;
        if state.role != ReplicaRole::Primary || authority.primary_identity() != &self.identity {
            return Err(RuntimeError::NotPrimary);
        }
        let local_committed_lsn = state.committed_lsn;
        drop(state);

        let replicator = self.replicator.lock().await;
        let boundary = replicator.highest_lsn();
        let committed_lsn = local_committed_lsn.max(replicator.committed_lsn());
        let retained = replicator.retained_operations_from(1);
        drop(replicator);
        let build_authority = BuildAuthority {
            build_id,
            source: self.identity.clone(),
            target,
            current_configuration: authority.current_configuration,
            replication_boundary_lsn: boundary,
        };
        build_authority.validate()?;

        let mut operations = BTreeMap::new();
        let mut stream = self.application.copy_operations(1).await?;
        while let Some(operation) = stream.next().await {
            let operation = operation?;
            if operation.lsn <= boundary {
                insert_copy_operation(&mut operations, operation)?;
            }
        }
        for operation in retained
            .into_iter()
            .filter(|operation| operation.lsn <= boundary)
        {
            insert_copy_operation(&mut operations, operation)?;
        }
        if boundary > 0 && (1..=boundary).any(|lsn| !operations.contains_key(&lsn)) {
            return Err(RuntimeError::InvalidReplication(
                "copy stream and retained queue do not close the replication gap".to_string(),
            ));
        }

        let mut sequence = 1_u64;
        let mut items = operations
            .into_values()
            .map(|operation| {
                let item = proto::CopyItem {
                    protocol_version: kuberic_protocol::PROTOCOL_VERSION,
                    build_id: build_authority.build_id.to_string(),
                    sender: Some(build_authority.source.clone().into()),
                    receiver: Some(build_authority.target.clone().into()),
                    epoch: Some(build_authority.current_configuration.epoch.into()),
                    current_configuration_id: build_authority
                        .current_configuration
                        .configuration_id
                        .to_string(),
                    sequence,
                    lsn: operation.lsn,
                    committed_lsn: operation.committed_lsn.min(operation.lsn),
                    replication_boundary_lsn: boundary,
                    final_item: false,
                    data: operation.data.to_vec(),
                };
                sequence += 1;
                item
            })
            .collect::<Vec<_>>();
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
            sequence,
            lsn: boundary,
            committed_lsn: committed_lsn.min(boundary),
            replication_boundary_lsn: boundary,
            final_item: true,
            data: Vec::new(),
        });
        let emitted = items
            .iter()
            .map(|item| {
                (
                    item.sequence,
                    EmittedBuildItem {
                        lsn: item.lsn,
                        final_item: item.final_item,
                    },
                )
            })
            .collect();
        self.state.write().await.outbound_builds.insert(
            build_authority.build_id.clone(),
            OutboundBuild {
                progress: BuildProgress {
                    authority: build_authority.clone(),
                    last_sequence: 0,
                    durable_lsn: 0,
                    completed: false,
                },
                final_sequence: sequence,
                next_sequence: sequence + 1,
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

        let durable_lsn = if envelope.sequence <= progress.last_sequence {
            if envelope.final_item {
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
            if envelope.final_item {
                if progress.durable_lsn != envelope.replication_boundary_lsn {
                    return Err(RuntimeError::InvalidReplication(
                        "final copy marker arrived before the replication boundary was durable"
                            .to_string(),
                    ));
                }
                let durable = self.application.commit(envelope.committed_lsn).await?;
                if durable.applied_lsn < envelope.replication_boundary_lsn {
                    return Err(RuntimeError::Application(
                        "application lost the durable copy boundary".to_string(),
                    ));
                }
                let mut state = self.state.write().await;
                let build = state
                    .builds
                    .get_mut(&envelope.build_id)
                    .expect("build authority remains installed");
                build.last_sequence = envelope.sequence;
                build.durable_lsn = durable.applied_lsn;
                build.completed = true;
                state.current_progress = state.current_progress.max(durable.applied_lsn);
                state.committed_lsn = state.committed_lsn.max(durable.committed_lsn);
                durable.applied_lsn
            } else {
                if envelope.lsn != progress.durable_lsn + 1 {
                    return Err(RuntimeError::InvalidReplication(format!(
                        "copy LSN gap: expected {}, observed {}",
                        progress.durable_lsn + 1,
                        envelope.lsn
                    )));
                }
                let durable = self
                    .application
                    .apply(Operation {
                        lsn: envelope.lsn,
                        committed_lsn: envelope.committed_lsn,
                        data: Bytes::from(envelope.data.clone()),
                    })
                    .await?;
                validate_durable_ack(envelope.lsn, envelope.committed_lsn, durable)?;
                let mut state = self.state.write().await;
                let build = state
                    .builds
                    .get_mut(&envelope.build_id)
                    .expect("build authority remains installed");
                build.last_sequence = envelope.sequence;
                build.durable_lsn = durable.applied_lsn;
                state.current_progress = state.current_progress.max(durable.applied_lsn);
                state.committed_lsn = state.committed_lsn.max(durable.committed_lsn);
                durable.applied_lsn
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
        let role = state.role;
        let current_progress = state.current_progress;
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
        let operation = Operation {
            lsn: envelope.lsn,
            committed_lsn: envelope.committed_lsn,
            data: Bytes::from(envelope.data.clone()),
        };
        let digest = operation_digest(&operation);
        let durable_ack = if envelope.lsn <= current_progress {
            if !self.application.verify_applied(&operation).await? {
                return Err(RuntimeError::InvalidReplication(
                    "duplicate LSN has different durable contents".to_string(),
                ));
            }
            let progress = self.application.durable_progress().await?;
            if envelope.committed_lsn > progress.committed_lsn {
                self.application.commit(envelope.committed_lsn).await?
            } else {
                progress
            }
        } else {
            if envelope.lsn != current_progress + 1 {
                return Err(RuntimeError::InvalidReplication(format!(
                    "replication gap: expected LSN {}, observed {}",
                    current_progress + 1,
                    envelope.lsn
                )));
            }
            let durable_ack = self.application.apply(operation).await?;
            validate_durable_ack(envelope.lsn, envelope.committed_lsn, durable_ack)?;
            durable_ack
        };
        {
            let mut state = self.state.write().await;
            if let Some(previous_digest) = state.received_digests.get(&envelope.lsn)
                && previous_digest != &digest
            {
                return Err(RuntimeError::InvalidReplication(
                    "duplicate LSN has conflicting payload".to_string(),
                ));
            }
            state.received_digests.insert(envelope.lsn, digest);
            state.current_progress = durable_ack.applied_lsn;
            state.committed_lsn = state.committed_lsn.max(durable_ack.committed_lsn);
        }
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
            applied_lsn: durable_ack.applied_lsn,
            committed_lsn: durable_ack.committed_lsn,
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
            committed_lsn: snapshot.5.max(replicator.committed_lsn()),
            current_configuration_quorum_progress: replicator
                .current_configuration_quorum_progress(),
            catch_up_boundary: replicator.catch_up_boundary(),
            catch_up_complete: replicator.catch_up_complete(),
            builds: snapshot.6,
        }
    }

    async fn execute_action(&self, action: RuntimeEffectAction) -> Result<()> {
        match action {
            RuntimeEffectAction::Open => {
                self.application.open().await?;
                let progress = self.application.durable_progress().await?;
                let mut state = self.state.write().await;
                state.open = true;
                state.current_progress = progress.applied_lsn;
                state.committed_lsn = progress.committed_lsn;
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
                self.state.write().await.authority = Some(authority);
            }
            RuntimeEffectAction::AdmitBuildAuthority(authority) => {
                let authority = *authority;
                authority.validate()?;
                if authority.target != self.identity {
                    return Err(RuntimeError::AuthorityMismatch(
                        "build authority target differs from runtime identity".to_string(),
                    ));
                }
                self.authority_store.admit_build(&authority).await?;
                let durable_lsn = self.state.read().await.current_progress;
                self.state.write().await.builds.insert(
                    authority.build_id.clone(),
                    BuildProgress {
                        authority,
                        last_sequence: 0,
                        durable_lsn,
                        completed: false,
                    },
                );
            }
            RuntimeEffectAction::ChangeRole(role) => {
                if !self.state.read().await.open {
                    return Err(RuntimeError::NotOpen);
                }
                self.state.write().await.write_status = AccessStatus::ReconfigurationPending;
                if role != ReplicaRole::Primary {
                    self.replicator.lock().await.fence_client_writes();
                }
                self.application.change_role(role).await?;
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
            committed_lsn: postcondition.5.max(replicator.committed_lsn()),
            current_configuration_quorum_progress: replicator
                .current_configuration_quorum_progress(),
            catch_up_boundary: replicator.catch_up_boundary(),
            catch_up_complete: replicator.catch_up_complete(),
            builds: postcondition.6,
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
        self.replicator.lock().await.finalize_commit(ready_lsn)?;
        let mut state = self.state.write().await;
        state.committed_lsn = state.committed_lsn.max(progress.committed_lsn);
        Ok(())
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

fn operation_digest(operation: &Operation) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(operation.lsn.to_be_bytes());
    hasher.update(operation.data.as_ref());
    hasher.finalize().into()
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
