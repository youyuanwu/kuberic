use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream;
use kuberic_protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationDescriptor, ConfigurationMember, Epoch,
    OperationId, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole, TransitionKind,
};
use kuberic_runtime::application::{
    ClientWrite, CopyChunk, CopyStream, DurableApplicationAck, DurableApplicationProgress,
    OpenContext, OpenMode, Operation, OperationStream, RoleChange, StateProvider,
    StatefulServiceReplica,
};
use kuberic_runtime::authority::{
    AdmittedAuthority, AuthorityFence, AuthorityStore, BuildAuthority, BuildAuthorityKind,
    DurableBuildProgress, DurableLocalWrite, ReplicationProgress,
};
use kuberic_runtime::effects::{
    RuntimeControlPlane, RuntimeEffect, RuntimeEffectAction, RuntimeEffectResult,
};
use kuberic_runtime::replicator::copy::{BuildConfiguration, PrepareCopyRequest};
use kuberic_runtime::runtime::PodRuntime;
use kuberic_runtime::{Result, RuntimeError};
use kuberic_wire::proto;
use tokio::sync::Notify;

#[derive(Default)]
struct MemoryAuthorityStore {
    authority: Mutex<Option<AdmittedAuthority>>,
    replication_progress: Mutex<BTreeMap<AuthorityFence, ReplicationProgress>>,
    local_writes: Mutex<BTreeMap<OperationId, DurableLocalWrite>>,
    builds: Mutex<BTreeMap<OperationId, BuildAuthority>>,
    build_progress: Mutex<BTreeMap<OperationId, DurableBuildProgress>>,
    admit_count: AtomicUsize,
    fail_build_progress_once: AtomicBool,
    fail_registered_write_once: AtomicBool,
    pause_registered_write: AtomicBool,
    registered_write_notify: Notify,
    resume_registered_write_notify: Notify,
    pause_committed_write: AtomicBool,
    committed_write_notify: Notify,
    resume_committed_write_notify: Notify,
}

#[async_trait]
impl AuthorityStore for MemoryAuthorityStore {
    async fn load(&self) -> Result<Option<AdmittedAuthority>> {
        Ok(self.authority.lock().unwrap().clone())
    }

    async fn admit(&self, authority: &AdmittedAuthority) -> Result<()> {
        self.admit_count.fetch_add(1, Ordering::SeqCst);
        *self.authority.lock().unwrap() = Some(authority.clone());
        Ok(())
    }

    async fn load_replication_progress(
        &self,
        fence: &AuthorityFence,
    ) -> Result<Option<ReplicationProgress>> {
        Ok(self
            .replication_progress
            .lock()
            .unwrap()
            .get(fence)
            .cloned())
    }

    async fn load_configuration_progress(
        &self,
        epoch: Epoch,
        current_configuration_id: &kuberic_protocol::types::ConfigurationId,
    ) -> Result<Option<ReplicationProgress>> {
        Ok(self
            .replication_progress
            .lock()
            .unwrap()
            .values()
            .filter(|progress| {
                progress.fence.epoch == epoch
                    && &progress.fence.current_configuration_id == current_configuration_id
            })
            .max_by_key(|progress| progress.verified_lsn)
            .cloned())
    }

    async fn record_replication_progress(&self, progress: &ReplicationProgress) -> Result<()> {
        self.replication_progress
            .lock()
            .unwrap()
            .insert(progress.fence.clone(), progress.clone());
        Ok(())
    }

    async fn load_local_write(
        &self,
        operation_id: &OperationId,
    ) -> Result<Option<DurableLocalWrite>> {
        Ok(self.local_writes.lock().unwrap().get(operation_id).cloned())
    }

    async fn load_local_writes(&self) -> Result<Vec<DurableLocalWrite>> {
        Ok(self
            .local_writes
            .lock()
            .unwrap()
            .values()
            .filter(|write| write.phase != kuberic_runtime::authority::LocalWritePhase::Committed)
            .cloned()
            .collect())
    }

    async fn record_local_write(&self, write: &DurableLocalWrite) -> Result<()> {
        if write.phase == kuberic_runtime::authority::LocalWritePhase::Registered
            && self
                .fail_registered_write_once
                .swap(false, Ordering::SeqCst)
        {
            return Err(RuntimeError::Application(
                "injected registered-write persistence failure".to_string(),
            ));
        }
        self.local_writes
            .lock()
            .unwrap()
            .insert(write.operation_id.clone(), write.clone());
        if write.phase == kuberic_runtime::authority::LocalWritePhase::Registered
            && self.pause_registered_write.load(Ordering::SeqCst)
        {
            self.registered_write_notify.notify_one();
            self.resume_registered_write_notify.notified().await;
        }
        if write.phase == kuberic_runtime::authority::LocalWritePhase::Committed
            && self.pause_committed_write.load(Ordering::SeqCst)
        {
            self.committed_write_notify.notify_one();
            self.resume_committed_write_notify.notified().await;
        }
        Ok(())
    }

    async fn load_build(&self, build_id: &OperationId) -> Result<Option<BuildAuthority>> {
        Ok(self.builds.lock().unwrap().get(build_id).cloned())
    }

    async fn admit_build(&self, authority: &BuildAuthority) -> Result<()> {
        let mut builds = self.builds.lock().unwrap();
        if let Some(existing) = builds.get(&authority.build_id)
            && existing != authority
        {
            return Err(RuntimeError::AuthorityMismatch(
                "test store rejected conflicting build authority".to_string(),
            ));
        }
        builds.insert(authority.build_id.clone(), authority.clone());
        Ok(())
    }

    async fn load_build_progress(
        &self,
        build_id: &OperationId,
    ) -> Result<Option<DurableBuildProgress>> {
        Ok(self.build_progress.lock().unwrap().get(build_id).cloned())
    }

    async fn record_build_progress(&self, progress: &DurableBuildProgress) -> Result<()> {
        if self.fail_build_progress_once.swap(false, Ordering::SeqCst) {
            return Err(RuntimeError::Application(
                "injected build progress failure".to_string(),
            ));
        }
        self.build_progress
            .lock()
            .unwrap()
            .insert(progress.authority.build_id.clone(), progress.clone());
        Ok(())
    }
}

#[derive(Default)]
struct TestApplication {
    applied: Mutex<BTreeMap<i64, Operation>>,
    progress: Mutex<DurableApplicationProgress>,
    copy_chunks: Mutex<BTreeMap<(String, u64), Bytes>>,
    fail_apply: AtomicBool,
    fail_after_apply: AtomicBool,
    pause_after_apply: AtomicBool,
    applied_notify: Notify,
    resume_notify: Notify,
    fail_commit: AtomicBool,
    pause_commit: AtomicBool,
    commit_notify: Notify,
    resume_commit_notify: Notify,
}

impl TestApplication {
    fn seed_progress(&self, applied_lsn: i64) {
        let mut progress = self.progress.lock().unwrap();
        let mut operations = self.applied.lock().unwrap();
        for lsn in (progress.applied_lsn + 1)..=applied_lsn {
            operations.insert(
                lsn,
                Operation {
                    lsn,
                    committed_lsn: lsn,
                    data: Bytes::from(format!("seed-{lsn}")),
                },
            );
        }
        progress.applied_lsn = applied_lsn;
        progress.committed_lsn = progress.committed_lsn.max(applied_lsn);
    }

    fn seed_operation(&self, lsn: i64, data: Bytes) {
        let mut progress = self.progress.lock().unwrap();
        let mut operations = self.applied.lock().unwrap();
        assert_eq!(lsn, progress.applied_lsn + 1);
        operations.insert(
            lsn,
            Operation {
                lsn,
                committed_lsn: lsn,
                data,
            },
        );
        progress.applied_lsn = lsn;
        progress.committed_lsn = lsn;
    }
}

#[derive(Default)]
struct TestControlPlane {
    effects: VecDeque<RuntimeEffect>,
    published: Vec<RuntimeEffectResult>,
}

#[async_trait]
impl RuntimeControlPlane for TestControlPlane {
    async fn next_effect(&mut self) -> Result<Option<RuntimeEffect>> {
        Ok(self.effects.pop_front())
    }

    async fn publish(&mut self, result: RuntimeEffectResult) -> Result<()> {
        self.published.push(result);
        Ok(())
    }
}

#[async_trait]
impl StatefulServiceReplica for TestApplication {
    async fn open(&self, _context: OpenContext) -> Result<()> {
        Ok(())
    }

    async fn change_role(&self, _role: ReplicaRole) -> Result<RoleChange> {
        Ok(RoleChange {
            service_address: None,
        })
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }

    fn abort(&self) {}
}

#[async_trait]
impl StateProvider for TestApplication {
    async fn update_epoch(&self, _epoch: Epoch, _previous_epoch_last_lsn: i64) -> Result<()> {
        Ok(())
    }

    async fn last_committed_lsn(&self) -> Result<i64> {
        Ok(self.progress.lock().unwrap().committed_lsn)
    }

    async fn get_copy_context(&self) -> Result<Bytes> {
        Ok(Bytes::new())
    }

    async fn get_copy_state(&self, up_to_lsn: i64, _copy_context: Bytes) -> Result<CopyStream> {
        let mut snapshot = Vec::new();
        for operation in self
            .applied
            .lock()
            .unwrap()
            .range(..=up_to_lsn)
            .map(|(_, operation)| operation)
        {
            snapshot.extend_from_slice(&operation.lsn.to_be_bytes());
            snapshot.extend_from_slice(&operation.committed_lsn.to_be_bytes());
            snapshot.extend_from_slice(&(operation.data.len() as u64).to_be_bytes());
            snapshot.extend_from_slice(&operation.data);
        }
        let chunks = if snapshot.is_empty() {
            Vec::new()
        } else {
            let split = snapshot.len().div_ceil(2);
            vec![
                Ok(CopyChunk {
                    data: Bytes::copy_from_slice(&snapshot[..split]),
                }),
                Ok(CopyChunk {
                    data: Bytes::copy_from_slice(&snapshot[split..]),
                }),
            ]
        };
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn get_replication_operations(
        &self,
        from_lsn: i64,
        to_lsn: i64,
    ) -> Result<OperationStream> {
        if from_lsn > to_lsn {
            return Ok(Box::pin(stream::empty()));
        }
        let operations = self
            .applied
            .lock()
            .unwrap()
            .range(from_lsn..=to_lsn)
            .map(|(_, operation)| Ok(operation.clone()))
            .collect::<Vec<_>>();
        Ok(Box::pin(stream::iter(operations)))
    }

    async fn apply_copy_chunk(
        &self,
        build_id: &OperationId,
        sequence: u64,
        chunk: CopyChunk,
    ) -> Result<()> {
        let key = (build_id.to_string(), sequence);
        let mut chunks = self.copy_chunks.lock().unwrap();
        if let Some(existing) = chunks.get(&key)
            && existing != &chunk.data
        {
            return Err(RuntimeError::Application(
                "copy chunk retry changed contents".to_string(),
            ));
        }
        chunks.insert(key, chunk.data);
        Ok(())
    }

    async fn finish_copy(
        &self,
        build_id: &OperationId,
        up_to_lsn: i64,
        committed_lsn: i64,
    ) -> Result<DurableApplicationProgress> {
        let prefix = build_id.to_string();
        let chunks = self.copy_chunks.lock().unwrap();
        let mut copied = chunks
            .iter()
            .filter(|((id, _), _)| id == &prefix)
            .map(|((_, sequence), data)| (*sequence, data.clone()))
            .collect::<Vec<_>>();
        copied.sort_by_key(|(sequence, _)| *sequence);
        drop(chunks);
        let mut snapshot = Vec::new();
        for (_, chunk) in copied {
            snapshot.extend_from_slice(&chunk);
        }
        let mut operations = self.applied.lock().unwrap();
        let mut offset = 0;
        while offset < snapshot.len() {
            if snapshot.len() - offset < 24 {
                return Err(RuntimeError::Application(
                    "copy chunk is malformed".to_string(),
                ));
            }
            let lsn = i64::from_be_bytes(snapshot[offset..offset + 8].try_into().unwrap());
            let operation_committed_lsn =
                i64::from_be_bytes(snapshot[offset + 8..offset + 16].try_into().unwrap());
            let data_len =
                u64::from_be_bytes(snapshot[offset + 16..offset + 24].try_into().unwrap()) as usize;
            offset += 24;
            if snapshot.len() - offset < data_len {
                return Err(RuntimeError::Application(
                    "copy chunk data is truncated".to_string(),
                ));
            }
            operations.insert(
                lsn,
                Operation {
                    lsn,
                    committed_lsn: operation_committed_lsn,
                    data: Bytes::copy_from_slice(&snapshot[offset..offset + data_len]),
                },
            );
            offset += data_len;
        }
        drop(operations);
        let mut progress = self.progress.lock().unwrap();
        progress.applied_lsn = progress.applied_lsn.max(up_to_lsn);
        progress.committed_lsn = progress.committed_lsn.max(committed_lsn);
        Ok(*progress)
    }

    async fn on_data_loss(&self) -> Result<bool> {
        Ok(false)
    }

    async fn apply(&self, operation: Operation) -> Result<DurableApplicationAck> {
        if self.fail_apply.load(Ordering::SeqCst) {
            return Err(RuntimeError::Application(
                "injected durable apply failure".to_string(),
            ));
        }
        let result = {
            let mut progress = self.progress.lock().unwrap();
            if operation.lsn != progress.applied_lsn + 1 {
                return Err(RuntimeError::Application(
                    "test application observed a replication gap".to_string(),
                ));
            }
            progress.applied_lsn = operation.lsn;
            progress.committed_lsn = progress.committed_lsn.max(operation.committed_lsn);
            self.applied
                .lock()
                .unwrap()
                .insert(operation.lsn, operation);
            *progress
        };
        if self.pause_after_apply.load(Ordering::SeqCst) {
            self.applied_notify.notify_one();
            self.resume_notify.notified().await;
        }
        if self.fail_after_apply.swap(false, Ordering::SeqCst) {
            return Err(RuntimeError::Application(
                "injected ambiguous apply failure".to_string(),
            ));
        }
        Ok(result)
    }

    async fn durable_progress(&self) -> Result<DurableApplicationProgress> {
        Ok(*self.progress.lock().unwrap())
    }

    async fn verify_applied(&self, operation: &Operation) -> Result<bool> {
        Ok(self
            .applied
            .lock()
            .unwrap()
            .get(&operation.lsn)
            .is_some_and(|applied| applied.data == operation.data))
    }

    async fn commit(&self, committed_lsn: i64) -> Result<DurableApplicationProgress> {
        let result = {
            let mut progress = self.progress.lock().unwrap();
            if committed_lsn > progress.applied_lsn {
                return Err(RuntimeError::Application(
                    "commit exceeds applied progress".to_string(),
                ));
            }
            progress.committed_lsn = progress.committed_lsn.max(committed_lsn);
            *progress
        };
        if self.pause_commit.load(Ordering::SeqCst) {
            self.commit_notify.notify_one();
            self.resume_commit_notify.notified().await;
        }
        if self.fail_commit.swap(false, Ordering::SeqCst) {
            return Err(RuntimeError::Application(
                "injected commit failure".to_string(),
            ));
        }
        Ok(result)
    }
}

fn identity(id: i64, instance: &str) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(instance),
        agent_generation: AgentGeneration::new(format!("generation-{instance}")),
    }
}

fn authority(local: ReplicaIdentity, members: Vec<ReplicaIdentity>) -> AdmittedAuthority {
    let write_quorum = members.len() as u32 / 2 + 1;
    let configuration = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        members[0].replica_id,
        members
            .into_iter()
            .enumerate()
            .map(|(index, identity)| ConfigurationMember {
                identity,
                role: if index == 0 {
                    ReplicaRole::Primary
                } else {
                    ReplicaRole::ActiveSecondary
                },
            })
            .collect(),
        write_quorum,
    );
    AdmittedAuthority {
        local_identity: local,
        transition_kind: None,
        previous_configuration: None,
        current_configuration: configuration,
    }
}

fn effect(sequence: u64, action: RuntimeEffectAction) -> RuntimeEffect {
    RuntimeEffect {
        operation_id: OperationId::new(format!("effect-{sequence}")),
        sequence,
        action,
    }
}

fn acknowledgement(
    authority: &AdmittedAuthority,
    receiver: ReplicaIdentity,
    lsn: i64,
) -> proto::ReplicationAck {
    proto::ReplicationAck {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        sender: Some(authority.primary_identity().clone().into()),
        receiver: Some(receiver.into()),
        epoch: Some(authority.current_configuration.epoch.into()),
        previous_configuration_id: authority
            .previous_configuration
            .as_ref()
            .map_or_else(String::new, |configuration| {
                configuration.configuration_id.to_string()
            }),
        current_configuration_id: authority.current_configuration.configuration_id.to_string(),
        received_lsn: lsn,
        applied_lsn: lsn,
        committed_lsn: 0,
    }
}

#[tokio::test]
async fn direct_writes_require_primary_role_and_explicit_write_grant() {
    let local = identity(1, "single");
    let application = Arc::new(TestApplication::default());
    let store = Arc::new(MemoryAuthorityStore::default());
    let runtime = PodRuntime::new(local.clone(), application.clone(), store);

    assert!(matches!(
        runtime
            .begin_write(ClientWrite {
                operation_id: OperationId::new("closed"),
                data: Bytes::from_static(b"closed")
            })
            .await,
        Err(RuntimeError::NotOpen)
    ));
    runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(authority(
                local.clone(),
                vec![local.clone()],
            ))),
        ))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::ChangeRole(ReplicaRole::Primary),
        ))
        .await
        .unwrap();
    let primary_snapshot = runtime.snapshot().await;
    assert_eq!(primary_snapshot.role, ReplicaRole::Primary);
    assert_eq!(
        primary_snapshot.write_status,
        AccessStatus::ReconfigurationPending
    );

    assert!(matches!(
        runtime
            .begin_write(ClientWrite {
                operation_id: OperationId::new("still-closed"),
                data: Bytes::from_static(b"still-closed")
            })
            .await,
        Err(RuntimeError::WriteClosed(
            AccessStatus::ReconfigurationPending
        ))
    ));

    runtime
        .apply_effect(effect(
            4,
            RuntimeEffectAction::SetWriteStatus(AccessStatus::Granted),
        ))
        .await
        .unwrap();
    let pending = runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("committed"),
            data: Bytes::from_static(b"committed"),
        })
        .await
        .unwrap();
    assert!(pending.replication_items.is_empty());
    assert_eq!(
        application.durable_progress().await.unwrap().committed_lsn,
        1
    );
    assert_eq!(pending.committed().await.unwrap().committed_lsn, 1);
}

#[tokio::test]
async fn local_write_retries_the_same_lsn_after_definite_or_ambiguous_failure() {
    let local = identity(1, "single");
    let application = Arc::new(TestApplication::default());
    let store = Arc::new(MemoryAuthorityStore::default());
    let runtime = Arc::new(PodRuntime::new(
        local.clone(),
        application.clone(),
        store.clone(),
    ));
    runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(authority(local.clone(), vec![local]))),
        ))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::ChangeRole(ReplicaRole::Primary),
        ))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            4,
            RuntimeEffectAction::SetWriteStatus(AccessStatus::Granted),
        ))
        .await
        .unwrap();

    let first = ClientWrite {
        operation_id: OperationId::new("first"),
        data: Bytes::from_static(b"first"),
    };
    application.fail_apply.store(true, Ordering::SeqCst);
    assert!(matches!(
        runtime.begin_write(first.clone()).await,
        Err(RuntimeError::Application(_))
    ));
    application.fail_apply.store(false, Ordering::SeqCst);
    let retry = runtime.begin_write(first).await.unwrap();
    assert_eq!(retry.lsn, 1);
    retry.committed().await.unwrap();

    let second = ClientWrite {
        operation_id: OperationId::new("second"),
        data: Bytes::from_static(b"second"),
    };
    application.fail_after_apply.store(true, Ordering::SeqCst);
    assert!(matches!(
        runtime.begin_write(second.clone()).await,
        Err(RuntimeError::Application(_))
    ));
    assert_eq!(application.durable_progress().await.unwrap().applied_lsn, 2);
    let retry = runtime.begin_write(second).await.unwrap();
    assert_eq!(retry.lsn, 2);
    retry.committed().await.unwrap();

    let third = ClientWrite {
        operation_id: OperationId::new("third"),
        data: Bytes::from_static(b"third"),
    };
    application.pause_after_apply.store(true, Ordering::SeqCst);
    let runtime_task = runtime.clone();
    let third_task = tokio::spawn(async move { runtime_task.begin_write(third.clone()).await });
    application.applied_notify.notified().await;
    third_task.abort();
    assert!(matches!(third_task.await, Err(error) if error.is_cancelled()));
    application.pause_after_apply.store(false, Ordering::SeqCst);
    application.resume_notify.notify_waiters();
    let retry = runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("third"),
            data: Bytes::from_static(b"third"),
        })
        .await
        .unwrap();
    assert_eq!(retry.lsn, 3);
    retry.committed().await.unwrap();

    let fourth = ClientWrite {
        operation_id: OperationId::new("fourth"),
        data: Bytes::from_static(b"fourth"),
    };
    application.fail_commit.store(true, Ordering::SeqCst);
    assert!(matches!(
        runtime.begin_write(fourth.clone()).await,
        Err(RuntimeError::Application(_))
    ));
    let retry = runtime.begin_write(fourth).await.unwrap();
    assert_eq!(retry.lsn, 4);
    retry.committed().await.unwrap();

    let fifth = ClientWrite {
        operation_id: OperationId::new("fifth"),
        data: Bytes::from_static(b"fifth"),
    };
    application.pause_commit.store(true, Ordering::SeqCst);
    let runtime_task = runtime.clone();
    let fifth_task = tokio::spawn(async move { runtime_task.begin_write(fifth).await });
    application.commit_notify.notified().await;
    fifth_task.abort();
    assert!(matches!(fifth_task.await, Err(error) if error.is_cancelled()));
    application.pause_commit.store(false, Ordering::SeqCst);
    application.resume_commit_notify.notify_waiters();
    let retry = runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("fifth"),
            data: Bytes::from_static(b"fifth"),
        })
        .await
        .unwrap();
    assert_eq!(retry.lsn, 5);
    retry.committed().await.unwrap();

    let sixth = ClientWrite {
        operation_id: OperationId::new("sixth"),
        data: Bytes::from_static(b"sixth"),
    };
    store
        .fail_registered_write_once
        .store(true, Ordering::SeqCst);
    assert!(matches!(
        runtime.begin_write(sixth.clone()).await,
        Err(RuntimeError::Application(_))
    ));
    assert!(matches!(
        runtime
            .begin_write(ClientWrite {
                operation_id: OperationId::new("blocked-by-sixth"),
                data: Bytes::from_static(b"blocked"),
            })
            .await,
        Err(RuntimeError::LocalWritePending(_))
    ));
    let retry = runtime.begin_write(sixth).await.unwrap();
    assert_eq!(retry.lsn, 6);
    retry.committed().await.unwrap();

    let seventh = ClientWrite {
        operation_id: OperationId::new("seventh"),
        data: Bytes::from_static(b"seventh"),
    };
    store.pause_registered_write.store(true, Ordering::SeqCst);
    let runtime_task = runtime.clone();
    let seventh_task = tokio::spawn(async move { runtime_task.begin_write(seventh).await });
    store.registered_write_notify.notified().await;
    seventh_task.abort();
    assert!(matches!(seventh_task.await, Err(error) if error.is_cancelled()));
    store.pause_registered_write.store(false, Ordering::SeqCst);
    store.resume_registered_write_notify.notify_waiters();
    let retry = runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("seventh"),
            data: Bytes::from_static(b"seventh"),
        })
        .await
        .unwrap();
    assert_eq!(retry.lsn, 7);
    retry.committed().await.unwrap();

    let eighth = ClientWrite {
        operation_id: OperationId::new("eighth"),
        data: Bytes::from_static(b"eighth"),
    };
    store.pause_committed_write.store(true, Ordering::SeqCst);
    let runtime_task = runtime.clone();
    let eighth_task = tokio::spawn(async move { runtime_task.begin_write(eighth).await });
    store.committed_write_notify.notified().await;
    eighth_task.abort();
    assert!(matches!(eighth_task.await, Err(error) if error.is_cancelled()));
    store.pause_committed_write.store(false, Ordering::SeqCst);
    store.resume_committed_write_notify.notify_waiters();
    let retry = runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("eighth"),
            data: Bytes::from_static(b"eighth"),
        })
        .await
        .unwrap();
    assert_eq!(retry.lsn, 8);
    retry.committed().await.unwrap();
    let ninth = runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("ninth"),
            data: Bytes::from_static(b"ninth"),
        })
        .await
        .unwrap();
    assert_eq!(ninth.lsn, 9);
    ninth.committed().await.unwrap();
}

#[tokio::test]
async fn receiver_ack_requires_durable_authority_and_application_acceptance() {
    let primary = identity(1, "primary");
    let local = identity(2, "secondary");
    let admitted = authority(
        local.clone(),
        vec![primary.clone(), local.clone(), identity(3, "third")],
    );
    let application = Arc::new(TestApplication::default());
    let store = Arc::new(MemoryAuthorityStore::default());
    let runtime = PodRuntime::new(local.clone(), application.clone(), store.clone());
    runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    let item = proto::ReplicationItem {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        sender: Some(primary.into()),
        epoch: Some(admitted.current_configuration.epoch.into()),
        previous_configuration_id: String::new(),
        current_configuration_id: admitted.current_configuration.configuration_id.to_string(),
        lsn: 1,
        committed_lsn: 0,
        data: b"value".to_vec(),
        receiver: Some(local.clone().into()),
    };

    assert!(matches!(
        runtime.receive_replication(item.clone()).await,
        Err(RuntimeError::AuthorityNotAdmitted)
    ));
    assert!(application.applied.lock().unwrap().is_empty());

    runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(admitted)),
        ))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::ChangeRole(ReplicaRole::ActiveSecondary),
        ))
        .await
        .unwrap();
    assert_eq!(store.admit_count.load(Ordering::SeqCst), 1);

    let mut gap = item.clone();
    gap.lsn = 2;
    gap.data = b"gap".to_vec();
    assert!(matches!(
        runtime.receive_replication(gap).await,
        Err(RuntimeError::InvalidReplication(_))
    ));
    assert!(application.applied.lock().unwrap().is_empty());

    application.fail_apply.store(true, Ordering::SeqCst);
    assert!(matches!(
        runtime.receive_replication(item.clone()).await,
        Err(RuntimeError::Application(_))
    ));
    application.fail_apply.store(false, Ordering::SeqCst);

    let acknowledgement = runtime.receive_replication(item.clone()).await.unwrap();
    assert_eq!(acknowledgement.received_lsn, 1);
    assert_eq!(acknowledgement.applied_lsn, 1);
    assert_eq!(application.applied.lock().unwrap().len(), 1);

    let retry = runtime.receive_replication(item.clone()).await.unwrap();
    assert_eq!(retry.applied_lsn, 1);
    assert_eq!(application.applied.lock().unwrap().len(), 1);

    let mut conflicting = item;
    conflicting.data = b"different".to_vec();
    assert!(matches!(
        runtime.receive_replication(conflicting).await,
        Err(RuntimeError::InvalidReplication(_))
    ));

    let mut committed = retry_item(&retry, local);
    committed.lsn = 2;
    committed.committed_lsn = 1;
    committed.data = b"next".to_vec();
    runtime.receive_replication(committed).await.unwrap();
    let snapshot = runtime.snapshot().await;
    assert_eq!(snapshot.current_progress, 2);
    assert_eq!(snapshot.committed_lsn, 1);
    assert_eq!(
        application.durable_progress().await.unwrap().committed_lsn,
        1
    );
}

#[tokio::test]
async fn new_authority_ack_does_not_credit_an_unverified_old_suffix() {
    let primary = identity(1, "primary");
    let members = vec![
        primary.clone(),
        identity(2, "second"),
        identity(3, "third"),
        identity(4, "fourth"),
        identity(5, "fifth"),
    ];
    let primary_authority = authority(primary.clone(), members.clone());
    let primary_application = Arc::new(TestApplication::default());
    primary_application.seed_operation(1, Bytes::from_static(b"one"));
    let primary_runtime = PodRuntime::new(
        primary,
        primary_application,
        Arc::new(MemoryAuthorityStore::default()),
    );
    primary_runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    primary_runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(primary_authority.clone())),
        ))
        .await
        .unwrap();
    primary_runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::ChangeRole(ReplicaRole::Primary),
        ))
        .await
        .unwrap();
    primary_runtime
        .apply_effect(effect(
            4,
            RuntimeEffectAction::SetWriteStatus(AccessStatus::Granted),
        ))
        .await
        .unwrap();
    let pending = primary_runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("new-two"),
            data: Bytes::from_static(b"new-two"),
        })
        .await
        .unwrap();
    let pending_items = pending.replication_items.clone();

    for secondary in members.iter().skip(1).take(2) {
        let application = Arc::new(TestApplication::default());
        application.seed_operation(1, Bytes::from_static(b"one"));
        application.seed_operation(2, Bytes::from_static(b"old-two"));
        let local_authority = AdmittedAuthority {
            local_identity: secondary.clone(),
            ..primary_authority.clone()
        };
        let runtime = PodRuntime::new(
            secondary.clone(),
            application,
            Arc::new(MemoryAuthorityStore::default()),
        );
        runtime
            .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
            .await
            .unwrap();
        runtime
            .apply_effect(effect(
                2,
                RuntimeEffectAction::AdmitAuthority(Box::new(local_authority)),
            ))
            .await
            .unwrap();
        runtime
            .apply_effect(effect(
                3,
                RuntimeEffectAction::ChangeRole(ReplicaRole::ActiveSecondary),
            ))
            .await
            .unwrap();
        let mut replay = pending_items
            .iter()
            .find(|item| {
                item.receiver
                    .as_ref()
                    .is_some_and(|receiver| receiver.replica_id == secondary.replica_id.value())
            })
            .unwrap()
            .clone();
        replay.lsn = 1;
        replay.committed_lsn = 1;
        replay.data = b"one".to_vec();
        let ack = runtime.receive_replication(replay).await.unwrap();
        assert_eq!(ack.applied_lsn, 1);
        primary_runtime.accept_acknowledgement(ack).await.unwrap();

        let conflicting = pending_items
            .iter()
            .find(|item| {
                item.receiver
                    .as_ref()
                    .is_some_and(|receiver| receiver.replica_id == secondary.replica_id.value())
            })
            .unwrap()
            .clone();
        assert!(matches!(
            runtime.receive_replication(conflicting).await,
            Err(RuntimeError::InvalidReplication(_))
        ));
    }

    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(20), pending.committed())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn failover_does_not_inherit_previous_primary_verification_credit() {
    let old_primary = identity(1, "old-primary");
    let local = identity(2, "local");
    let new_primary = identity(3, "new-primary");
    let fourth = identity(4, "fourth");
    let fifth = identity(5, "fifth");
    let previous = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        old_primary.replica_id,
        vec![
            ConfigurationMember {
                identity: old_primary.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: local.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: new_primary.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: fourth.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: fifth.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        3,
    );
    let current = ConfigurationDescriptor::new(
        Epoch::new(0, 2),
        new_primary.replica_id,
        vec![
            ConfigurationMember {
                identity: old_primary,
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: local.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: new_primary,
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: fourth,
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: fifth,
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        3,
    );
    let store = Arc::new(MemoryAuthorityStore::default());
    store
        .record_replication_progress(&ReplicationProgress {
            fence: AuthorityFence {
                epoch: previous.epoch,
                previous_configuration_id: None,
                current_configuration_id: previous.configuration_id.clone(),
            },
            verified_lsn: 2,
        })
        .await
        .unwrap();
    let application = Arc::new(TestApplication::default());
    application.seed_operation(1, Bytes::from_static(b"one"));
    application.seed_operation(2, Bytes::from_static(b"old-two"));
    let runtime = PodRuntime::new(local.clone(), application, store);
    runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(AdmittedAuthority {
                local_identity: local,
                transition_kind: Some(TransitionKind::Failover),
                previous_configuration: Some(previous),
                current_configuration: current,
            })),
        ))
        .await
        .unwrap();
    assert_eq!(runtime.snapshot().await.verified_replication_lsn, Some(0));
}

fn retry_item(
    acknowledgement: &proto::ReplicationAck,
    receiver: ReplicaIdentity,
) -> proto::ReplicationItem {
    proto::ReplicationItem {
        protocol_version: kuberic_protocol::PROTOCOL_VERSION,
        sender: acknowledgement.sender.clone(),
        receiver: Some(receiver.into()),
        epoch: acknowledgement.epoch,
        previous_configuration_id: acknowledgement.previous_configuration_id.clone(),
        current_configuration_id: acknowledgement.current_configuration_id.clone(),
        lsn: acknowledgement.received_lsn,
        committed_lsn: acknowledgement.committed_lsn,
        data: b"value".to_vec(),
    }
}

#[tokio::test]
async fn effects_are_ordered_and_idempotent() {
    let local = identity(1, "single");
    let runtime = PodRuntime::new(
        local,
        Arc::new(TestApplication::default()),
        Arc::new(MemoryAuthorityStore::default()),
    );
    let open = effect(1, RuntimeEffectAction::Open(OpenMode::Existing));
    let first = runtime.apply_effect(open.clone()).await.unwrap();
    let duplicate = runtime.apply_effect(open).await.unwrap();
    assert_eq!(duplicate, first);
    assert!(matches!(
        runtime
            .apply_effect(effect(1, RuntimeEffectAction::Close))
            .await,
        Err(RuntimeError::EffectConflict { sequence: 1 })
    ));

    let close = effect(2, RuntimeEffectAction::Close);
    runtime.apply_effect(close).await.unwrap();
    assert_eq!(
        runtime
            .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing),))
            .await
            .unwrap(),
        first
    );
    assert!(matches!(
        runtime
            .apply_effect(effect(4, RuntimeEffectAction::Open(OpenMode::Existing),))
            .await,
        Err(RuntimeError::EffectOutOfOrder {
            expected: 3,
            observed: 4
        })
    ));
}

#[tokio::test]
async fn caller_supplies_the_control_plane() {
    let local = identity(1, "single");
    let runtime = PodRuntime::new(
        local,
        Arc::new(TestApplication::default()),
        Arc::new(MemoryAuthorityStore::default()),
    );
    let mut control_plane = TestControlPlane {
        effects: VecDeque::from([effect(1, RuntimeEffectAction::Open(OpenMode::Existing))]),
        published: Vec::new(),
    };

    runtime.serve(&mut control_plane).await.unwrap();
    assert_eq!(control_plane.published.len(), 1);
    assert!(control_plane.published[0].postcondition.open);
}

#[tokio::test]
async fn close_fences_pending_client_writes_before_ack_processing() {
    let local = identity(1, "primary");
    let secondary = identity(2, "secondary");
    let third = identity(3, "third");
    let runtime = PodRuntime::new(
        local.clone(),
        Arc::new(TestApplication::default()),
        Arc::new(MemoryAuthorityStore::default()),
    );
    runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(authority(
                local,
                vec![identity(1, "primary"), secondary, third],
            ))),
        ))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::ChangeRole(ReplicaRole::Primary),
        ))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            4,
            RuntimeEffectAction::SetWriteStatus(AccessStatus::Granted),
        ))
        .await
        .unwrap();
    let pending = runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("pending"),
            data: Bytes::from_static(b"pending"),
        })
        .await
        .unwrap();
    let item = pending.replication_items[0].clone();

    runtime
        .apply_effect(effect(5, RuntimeEffectAction::Close))
        .await
        .unwrap();
    runtime
        .accept_acknowledgement(proto::ReplicationAck {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            sender: item.sender,
            receiver: item.receiver,
            epoch: item.epoch,
            previous_configuration_id: item.previous_configuration_id,
            current_configuration_id: item.current_configuration_id,
            received_lsn: item.lsn,
            applied_lsn: item.lsn,
            committed_lsn: 0,
        })
        .await
        .unwrap();
    assert!(matches!(
        pending.committed().await,
        Err(RuntimeError::WriteClosed(
            AccessStatus::ReconfigurationPending
        ))
    ));
}

#[tokio::test]
async fn exact_target_copy_closes_the_replication_gap_before_completion() {
    let source = identity(1, "source");
    let target = identity(1, "replacement");
    let source_runtime = PodRuntime::new(
        source.clone(),
        Arc::new(TestApplication::default()),
        Arc::new(MemoryAuthorityStore::default()),
    );
    source_runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    source_runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(authority(
                source.clone(),
                vec![source.clone()],
            ))),
        ))
        .await
        .unwrap();
    source_runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::ChangeRole(ReplicaRole::Primary),
        ))
        .await
        .unwrap();
    source_runtime
        .apply_effect(effect(
            4,
            RuntimeEffectAction::SetWriteStatus(AccessStatus::Granted),
        ))
        .await
        .unwrap();
    source_runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("one"),
            data: Bytes::from_static(b"one"),
        })
        .await
        .unwrap()
        .committed()
        .await
        .unwrap();
    let prepared = source_runtime
        .prepare_copy(PrepareCopyRequest {
            build_id: OperationId::new("build"),
            target: target.clone(),
            configuration: BuildConfiguration::Current,
            copy_context: Bytes::new(),
        })
        .await
        .unwrap();
    assert_eq!(prepared.authority.replication_boundary_lsn, 1);
    assert_eq!(prepared.items.len(), 3);
    let live_write = source_runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("two"),
            data: Bytes::from_static(b"two"),
        })
        .await
        .unwrap();
    assert_eq!(live_write.build_items.len(), 1);
    let live_build_item = live_write.build_items[0].clone();
    live_write.committed().await.unwrap();

    let target_application = Arc::new(TestApplication::default());
    let target_runtime = PodRuntime::new(
        target,
        target_application.clone(),
        Arc::new(MemoryAuthorityStore::default()),
    );
    target_runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    target_runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::ChangeRole(ReplicaRole::IdleSecondary),
        ))
        .await
        .unwrap();
    target_runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::AdmitBuildAuthority(Box::new(prepared.authority.clone())),
        ))
        .await
        .unwrap();

    assert!(matches!(
        target_runtime
            .receive_copy_item(prepared.items[2].clone())
            .await,
        Err(RuntimeError::InvalidReplication(_))
    ));
    let first_ack = target_runtime
        .receive_copy_item(prepared.items[0].clone())
        .await
        .unwrap();
    source_runtime
        .accept_copy_acknowledgement(first_ack.clone())
        .await
        .unwrap();
    assert!(!first_ack.final_item);
    target_runtime
        .receive_copy_item(prepared.items[0].clone())
        .await
        .unwrap();
    assert_eq!(target_application.applied.lock().unwrap().len(), 0);
    target_runtime
        .receive_copy_item(prepared.items[1].clone())
        .await
        .unwrap();
    let final_ack = target_runtime
        .receive_copy_item(prepared.items[2].clone())
        .await
        .unwrap();
    assert_eq!(target_application.applied.lock().unwrap().len(), 1);
    assert!(final_ack.final_item);
    assert_eq!(final_ack.durable_lsn, 1);
    let live_ack = target_runtime
        .receive_copy_item(live_build_item)
        .await
        .unwrap();
    let mut forged_ack = live_ack.clone();
    forged_ack.durable_lsn = 100;
    assert!(matches!(
        source_runtime.accept_copy_acknowledgement(forged_ack).await,
        Err(RuntimeError::InvalidReplication(_))
    ));
    source_runtime
        .accept_copy_acknowledgement(live_ack.clone())
        .await
        .unwrap();
    assert!(!source_runtime.snapshot().await.builds[0].completed);
    source_runtime
        .accept_copy_acknowledgement(final_ack)
        .await
        .unwrap();
    let retried_final = target_runtime
        .receive_copy_item(prepared.items[2].clone())
        .await
        .unwrap();
    assert_eq!(retried_final.durable_lsn, 1);
    source_runtime
        .accept_copy_acknowledgement(retried_final)
        .await
        .unwrap();
    let retried_chunk = target_runtime
        .receive_copy_item(prepared.items[0].clone())
        .await
        .unwrap();
    assert!(retried_chunk.snapshot_chunk);
    assert_eq!(retried_chunk.durable_lsn, 0);
    source_runtime
        .accept_copy_acknowledgement(retried_chunk)
        .await
        .unwrap();
    let snapshot = target_runtime.snapshot().await;
    assert_eq!(snapshot.current_progress, 2);
    assert!(snapshot.builds[0].completed);
    let source_snapshot = source_runtime.snapshot().await;
    assert!(source_snapshot.builds[0].completed);
    assert_eq!(source_snapshot.builds[0].durable_lsn, 2);
}

#[tokio::test]
async fn completed_copy_hands_off_to_new_authority_and_normal_quorum_replication() {
    let source = identity(1, "source");
    let secondary = identity(2, "secondary");
    let old = identity(3, "old");
    let replacement = identity(3, "replacement");
    let previous_authority =
        authority(source.clone(), vec![source.clone(), secondary.clone(), old]);
    let source_application = Arc::new(TestApplication::default());
    source_application.seed_operation(1, Bytes::from_static(b"one"));
    let source_runtime = PodRuntime::new(
        source.clone(),
        source_application,
        Arc::new(MemoryAuthorityStore::default()),
    );
    source_runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    source_runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(previous_authority.clone())),
        ))
        .await
        .unwrap();
    source_runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::ChangeRole(ReplicaRole::Primary),
        ))
        .await
        .unwrap();
    let prepared = source_runtime
        .prepare_copy(PrepareCopyRequest {
            build_id: OperationId::new("handoff-build"),
            target: replacement.clone(),
            configuration: BuildConfiguration::Current,
            copy_context: Bytes::new(),
        })
        .await
        .unwrap();

    let target_application = Arc::new(TestApplication::default());
    let target_store = Arc::new(MemoryAuthorityStore::default());
    let target_runtime = PodRuntime::new(replacement.clone(), target_application, target_store);
    target_runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::New)))
        .await
        .unwrap();
    target_runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::ChangeRole(ReplicaRole::IdleSecondary),
        ))
        .await
        .unwrap();
    target_runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::AdmitBuildAuthority(Box::new(prepared.authority.clone())),
        ))
        .await
        .unwrap();
    for item in prepared.items {
        target_runtime.receive_copy_item(item).await.unwrap();
    }

    let current = ConfigurationDescriptor::new(
        Epoch::new(0, 2),
        source.replica_id,
        vec![
            ConfigurationMember {
                identity: source.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: secondary.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: replacement.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    let new_source_authority = AdmittedAuthority {
        local_identity: source.clone(),
        transition_kind: Some(TransitionKind::Replacement),
        previous_configuration: Some(previous_authority.current_configuration.clone()),
        current_configuration: current.clone(),
    };
    let new_target_authority = AdmittedAuthority {
        local_identity: replacement.clone(),
        ..new_source_authority.clone()
    };
    target_runtime
        .apply_effect(effect(
            4,
            RuntimeEffectAction::AdmitAuthority(Box::new(new_target_authority)),
        ))
        .await
        .unwrap();
    target_runtime
        .apply_effect(effect(
            5,
            RuntimeEffectAction::ChangeRole(ReplicaRole::ActiveSecondary),
        ))
        .await
        .unwrap();
    assert_eq!(
        target_runtime.snapshot().await.verified_replication_lsn,
        Some(1)
    );

    let secondary_application = Arc::new(TestApplication::default());
    secondary_application.seed_operation(1, Bytes::from_static(b"one"));
    let secondary_store = Arc::new(MemoryAuthorityStore::default());
    secondary_store
        .record_replication_progress(&ReplicationProgress {
            fence: previous_authority.fence(),
            verified_lsn: 1,
        })
        .await
        .unwrap();
    let secondary_runtime =
        PodRuntime::new(secondary.clone(), secondary_application, secondary_store);
    secondary_runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    secondary_runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(AdmittedAuthority {
                local_identity: secondary.clone(),
                ..new_source_authority.clone()
            })),
        ))
        .await
        .unwrap();
    secondary_runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::ChangeRole(ReplicaRole::ActiveSecondary),
        ))
        .await
        .unwrap();

    source_runtime
        .apply_effect(effect(
            4,
            RuntimeEffectAction::AdmitAuthority(Box::new(new_source_authority.clone())),
        ))
        .await
        .unwrap();
    source_runtime
        .apply_effect(effect(
            5,
            RuntimeEffectAction::SetWriteStatus(AccessStatus::Granted),
        ))
        .await
        .unwrap();
    let pending = source_runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("two"),
            data: Bytes::from_static(b"two"),
        })
        .await
        .unwrap();
    for item in pending.replication_items.clone() {
        let receiver = item.receiver.as_ref().unwrap();
        let ack = if receiver.instance_id == replacement.instance_id.as_str() {
            target_runtime.receive_replication(item).await.unwrap()
        } else if receiver.instance_id == secondary.instance_id.as_str() {
            secondary_runtime.receive_replication(item).await.unwrap()
        } else {
            continue;
        };
        source_runtime.accept_acknowledgement(ack).await.unwrap();
    }
    assert_eq!(pending.committed().await.unwrap().committed_lsn, 2);

    target_runtime
        .apply_effect(effect(
            6,
            RuntimeEffectAction::AdmitAuthority(Box::new(AdmittedAuthority {
                local_identity: replacement.clone(),
                transition_kind: None,
                previous_configuration: None,
                current_configuration: current.clone(),
            })),
        ))
        .await
        .unwrap();
    assert_eq!(
        target_runtime.snapshot().await.verified_replication_lsn,
        Some(2)
    );
    source_runtime
        .apply_effect(effect(
            6,
            RuntimeEffectAction::AdmitAuthority(Box::new(AdmittedAuthority {
                local_identity: source,
                transition_kind: None,
                previous_configuration: None,
                current_configuration: current,
            })),
        ))
        .await
        .unwrap();
    let pending = source_runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("three"),
            data: Bytes::from_static(b"three"),
        })
        .await
        .unwrap();
    let item = pending
        .replication_items
        .iter()
        .find(|item| {
            item.receiver
                .as_ref()
                .is_some_and(|receiver| receiver.instance_id == replacement.instance_id.as_str())
        })
        .unwrap()
        .clone();
    let ack = target_runtime.receive_replication(item).await.unwrap();
    source_runtime.accept_acknowledgement(ack).await.unwrap();
    assert_eq!(pending.committed().await.unwrap().committed_lsn, 3);
}

#[tokio::test]
async fn bootstrap_primary_builds_full_genesis_members_before_configuration_admission() {
    let primary = identity(1, "primary");
    let secondary = identity(2, "secondary");
    let genesis = ConfigurationDescriptor::new(
        Epoch::new(0, 1),
        primary.replica_id,
        vec![
            ConfigurationMember {
                identity: primary.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: secondary.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: identity(3, "third"),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    let runtime = PodRuntime::new(
        primary,
        Arc::new(TestApplication::default()),
        Arc::new(MemoryAuthorityStore::default()),
    );
    runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::New)))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::ChangeRole(ReplicaRole::Primary),
        ))
        .await
        .unwrap();

    let prepared = runtime
        .prepare_copy(PrepareCopyRequest {
            build_id: OperationId::new("bootstrap-secondary"),
            target: secondary,
            configuration: BuildConfiguration::Bootstrap(genesis),
            copy_context: Bytes::new(),
        })
        .await
        .unwrap();
    assert_eq!(prepared.authority.kind, BuildAuthorityKind::Bootstrap);
    assert_eq!(prepared.authority.replication_boundary_lsn, 0);
    assert_eq!(prepared.items.len(), 1);
    assert!(prepared.items[0].final_item);
}

#[tokio::test]
async fn build_identity_is_immutable_and_recoverable_on_source_restart() {
    let source = identity(1, "source");
    let target = identity(1, "replacement");
    let admitted = authority(source.clone(), vec![source.clone()]);
    let application = Arc::new(TestApplication::default());
    let store = Arc::new(MemoryAuthorityStore::default());
    let runtime = PodRuntime::new(source.clone(), application.clone(), store.clone());
    runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(admitted.clone())),
        ))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::ChangeRole(ReplicaRole::Primary),
        ))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            4,
            RuntimeEffectAction::SetWriteStatus(AccessStatus::Granted),
        ))
        .await
        .unwrap();
    let request = PrepareCopyRequest {
        build_id: OperationId::new("immutable-build"),
        target: target.clone(),
        configuration: BuildConfiguration::Current,
        copy_context: Bytes::new(),
    };
    let first = runtime.prepare_copy(request.clone()).await.unwrap();
    assert_eq!(first.authority.replication_boundary_lsn, 0);
    runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("after-boundary"),
            data: Bytes::from_static(b"after-boundary"),
        })
        .await
        .unwrap()
        .committed()
        .await
        .unwrap();
    let retry = runtime.prepare_copy(request.clone()).await.unwrap();
    assert_eq!(retry.authority.replication_boundary_lsn, 0);
    assert!(
        retry
            .items
            .iter()
            .any(|item| item.lsn == 1 && !item.final_item)
    );

    let mut conflicting = request.clone();
    conflicting.target = identity(2, "different-target");
    assert!(matches!(
        runtime.prepare_copy(conflicting).await,
        Err(RuntimeError::AuthorityMismatch(_))
    ));

    let restarted = PodRuntime::new(source, application, store);
    restarted
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    restarted
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(admitted)),
        ))
        .await
        .unwrap();
    restarted
        .apply_effect(effect(
            3,
            RuntimeEffectAction::ChangeRole(ReplicaRole::Primary),
        ))
        .await
        .unwrap();
    let resumed = restarted.prepare_copy(request).await.unwrap();
    assert_eq!(resumed.authority.replication_boundary_lsn, 0);
    assert!(
        resumed
            .items
            .iter()
            .any(|item| item.lsn == 1 && !item.final_item)
    );
}

#[tokio::test]
async fn retrying_accepted_write_does_not_overwrite_build_final_sequence() {
    let source = identity(1, "source");
    let target = identity(1, "replacement");
    let application = Arc::new(TestApplication::default());
    let runtime = PodRuntime::new(
        source.clone(),
        application.clone(),
        Arc::new(MemoryAuthorityStore::default()),
    );
    runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(authority(source.clone(), vec![source]))),
        ))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::ChangeRole(ReplicaRole::Primary),
        ))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            4,
            RuntimeEffectAction::SetWriteStatus(AccessStatus::Granted),
        ))
        .await
        .unwrap();
    let write = ClientWrite {
        operation_id: OperationId::new("ambiguous-before-build"),
        data: Bytes::from_static(b"value"),
    };
    application.fail_after_apply.store(true, Ordering::SeqCst);
    assert!(matches!(
        runtime.begin_write(write.clone()).await,
        Err(RuntimeError::Application(_))
    ));
    let prepared = runtime
        .prepare_copy(PrepareCopyRequest {
            build_id: OperationId::new("final-sequence"),
            target,
            configuration: BuildConfiguration::Current,
            copy_context: Bytes::new(),
        })
        .await
        .unwrap();
    let final_item = prepared
        .items
        .iter()
        .find(|item| item.final_item)
        .unwrap()
        .clone();
    let retry = runtime.begin_write(write).await.unwrap();
    assert!(retry.build_items.is_empty());
    retry.committed().await.unwrap();
    runtime
        .accept_copy_acknowledgement(proto::CopyAck {
            protocol_version: kuberic_protocol::PROTOCOL_VERSION,
            build_id: final_item.build_id,
            sender: final_item.sender,
            receiver: final_item.receiver,
            epoch: final_item.epoch,
            current_configuration_id: final_item.current_configuration_id,
            sequence: final_item.sequence,
            durable_lsn: final_item.replication_boundary_lsn,
            replication_boundary_lsn: final_item.replication_boundary_lsn,
            final_item: true,
            snapshot_chunk: false,
        })
        .await
        .unwrap();
    assert!(runtime.snapshot().await.builds[0].completed);
}

#[tokio::test]
async fn copy_receiver_recovers_sequence_and_final_completion_after_restart() {
    let source = identity(1, "source");
    let target = identity(1, "replacement");
    let source_runtime = PodRuntime::new(
        source.clone(),
        Arc::new(TestApplication::default()),
        Arc::new(MemoryAuthorityStore::default()),
    );
    source_runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    source_runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(authority(source.clone(), vec![source]))),
        ))
        .await
        .unwrap();
    source_runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::ChangeRole(ReplicaRole::Primary),
        ))
        .await
        .unwrap();
    source_runtime
        .apply_effect(effect(
            4,
            RuntimeEffectAction::SetWriteStatus(AccessStatus::Granted),
        ))
        .await
        .unwrap();
    source_runtime
        .begin_write(ClientWrite {
            operation_id: OperationId::new("copy-value"),
            data: Bytes::from_static(b"copy-value"),
        })
        .await
        .unwrap()
        .committed()
        .await
        .unwrap();
    let prepared = source_runtime
        .prepare_copy(PrepareCopyRequest {
            build_id: OperationId::new("restartable-build"),
            target: target.clone(),
            configuration: BuildConfiguration::Current,
            copy_context: Bytes::new(),
        })
        .await
        .unwrap();

    let application = Arc::new(TestApplication::default());
    let store = Arc::new(MemoryAuthorityStore::default());
    let first_runtime = PodRuntime::new(target.clone(), application.clone(), store.clone());
    first_runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::New)))
        .await
        .unwrap();
    first_runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::ChangeRole(ReplicaRole::IdleSecondary),
        ))
        .await
        .unwrap();
    first_runtime
        .apply_effect(effect(
            3,
            RuntimeEffectAction::AdmitBuildAuthority(Box::new(prepared.authority.clone())),
        ))
        .await
        .unwrap();
    first_runtime
        .receive_copy_item(prepared.items[0].clone())
        .await
        .unwrap();

    let restarted = PodRuntime::new(target.clone(), application.clone(), store.clone());
    restarted
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    restarted
        .apply_effect(effect(
            2,
            RuntimeEffectAction::ChangeRole(ReplicaRole::IdleSecondary),
        ))
        .await
        .unwrap();
    restarted
        .apply_effect(effect(
            3,
            RuntimeEffectAction::AdmitBuildAuthority(Box::new(prepared.authority.clone())),
        ))
        .await
        .unwrap();
    restarted
        .receive_copy_item(prepared.items[0].clone())
        .await
        .unwrap();
    restarted
        .receive_copy_item(prepared.items[1].clone())
        .await
        .unwrap();
    store.fail_build_progress_once.store(true, Ordering::SeqCst);
    assert!(matches!(
        restarted.receive_copy_item(prepared.items[2].clone()).await,
        Err(RuntimeError::Application(_))
    ));

    let after_final_crash = PodRuntime::new(target, application, store);
    after_final_crash
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    after_final_crash
        .apply_effect(effect(
            2,
            RuntimeEffectAction::ChangeRole(ReplicaRole::IdleSecondary),
        ))
        .await
        .unwrap();
    after_final_crash
        .apply_effect(effect(
            3,
            RuntimeEffectAction::AdmitBuildAuthority(Box::new(prepared.authority)),
        ))
        .await
        .unwrap();
    let final_ack = after_final_crash
        .receive_copy_item(prepared.items[2].clone())
        .await
        .unwrap();
    assert!(final_ack.final_item);
    assert!(after_final_crash.snapshot().await.builds[0].completed);
}

#[tokio::test]
async fn runtime_exposes_cc_boundary_catch_up_evidence() {
    let primary = identity(1, "primary");
    let secondary = identity(2, "secondary");
    let old = identity(3, "old");
    let replacement = identity(3, "replacement");
    let previous = ConfigurationDescriptor::new(
        Epoch::new(0, 5),
        ReplicaId::new(1),
        vec![
            ConfigurationMember {
                identity: primary.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: secondary.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: old,
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    let current = ConfigurationDescriptor::new(
        Epoch::new(0, 6),
        ReplicaId::new(1),
        vec![
            ConfigurationMember {
                identity: primary.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: secondary.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: replacement,
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    let admitted = AdmittedAuthority {
        local_identity: primary.clone(),
        transition_kind: Some(TransitionKind::Replacement),
        previous_configuration: Some(previous),
        current_configuration: current,
    };
    let application = Arc::new(TestApplication::default());
    application.seed_progress(10);
    let runtime = PodRuntime::new(
        primary,
        application,
        Arc::new(MemoryAuthorityStore::default()),
    );
    runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(admitted.clone())),
        ))
        .await
        .unwrap();
    let waiting = runtime.snapshot().await;
    assert_eq!(waiting.catch_up_boundary, Some(10));
    assert!(!waiting.catch_up_complete);

    runtime
        .accept_acknowledgement(acknowledgement(&admitted, secondary, 10))
        .await
        .unwrap();
    let complete = runtime.snapshot().await;
    assert_eq!(complete.current_configuration_quorum_progress, 10);
    assert!(complete.catch_up_complete);
}

#[tokio::test]
async fn runtime_exposes_derived_must_catch_up_evidence() {
    let old_primary = identity(1, "old-primary");
    let new_primary = identity(2, "new-primary");
    let third = identity(3, "third");
    let previous = ConfigurationDescriptor::new(
        Epoch::new(0, 5),
        ReplicaId::new(1),
        vec![
            ConfigurationMember {
                identity: old_primary.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: new_primary.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: third.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    let current = ConfigurationDescriptor::new(
        Epoch::new(0, 6),
        ReplicaId::new(2),
        vec![
            ConfigurationMember {
                identity: old_primary.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
            ConfigurationMember {
                identity: new_primary.clone(),
                role: ReplicaRole::Primary,
            },
            ConfigurationMember {
                identity: third.clone(),
                role: ReplicaRole::ActiveSecondary,
            },
        ],
        2,
    );
    let admitted = AdmittedAuthority {
        local_identity: new_primary.clone(),
        transition_kind: Some(TransitionKind::Failover),
        previous_configuration: Some(previous),
        current_configuration: current,
    };
    let application = Arc::new(TestApplication::default());
    application.seed_progress(5);
    let runtime = PodRuntime::new(
        new_primary,
        application.clone(),
        Arc::new(MemoryAuthorityStore::default()),
    );
    runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open(OpenMode::Existing)))
        .await
        .unwrap();
    runtime
        .apply_effect(effect(
            2,
            RuntimeEffectAction::AdmitAuthority(Box::new(admitted.clone())),
        ))
        .await
        .unwrap();
    runtime
        .accept_acknowledgement(acknowledgement(&admitted, old_primary, 10))
        .await
        .unwrap();
    runtime
        .accept_acknowledgement(acknowledgement(&admitted, third, 10))
        .await
        .unwrap();
    let waiting = runtime.snapshot().await;
    assert_eq!(waiting.current_configuration_quorum_progress, 10);
    assert!(!waiting.catch_up_complete);

    application.seed_progress(10);
    runtime
        .apply_effect(effect(3, RuntimeEffectAction::RefreshApplicationProgress))
        .await
        .unwrap();
    assert!(runtime.snapshot().await.catch_up_complete);
}
