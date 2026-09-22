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
    ClientWrite, DurableApplicationAck, DurableApplicationProgress, Operation, OperationStream,
    StatefulApplication,
};
use kuberic_runtime::authority::{AdmittedAuthority, AuthorityStore, BuildAuthority};
use kuberic_runtime::effects::{
    RuntimeControlPlane, RuntimeEffect, RuntimeEffectAction, RuntimeEffectResult,
};
use kuberic_runtime::runtime::PodRuntime;
use kuberic_runtime::{Result, RuntimeError};
use kuberic_wire::proto;

#[derive(Default)]
struct MemoryAuthorityStore {
    authority: Mutex<Option<AdmittedAuthority>>,
    builds: Mutex<BTreeMap<OperationId, BuildAuthority>>,
    admit_count: AtomicUsize,
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

    async fn load_build(&self, build_id: &OperationId) -> Result<Option<BuildAuthority>> {
        Ok(self.builds.lock().unwrap().get(build_id).cloned())
    }

    async fn admit_build(&self, authority: &BuildAuthority) -> Result<()> {
        self.builds
            .lock()
            .unwrap()
            .insert(authority.build_id.clone(), authority.clone());
        Ok(())
    }
}

#[derive(Default)]
struct TestApplication {
    applied: Mutex<BTreeMap<i64, Operation>>,
    progress: Mutex<DurableApplicationProgress>,
    fail_apply: AtomicBool,
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
impl StatefulApplication for TestApplication {
    async fn open(&self) -> Result<()> {
        Ok(())
    }

    async fn change_role(&self, _role: ReplicaRole) -> Result<()> {
        Ok(())
    }

    async fn apply(&self, operation: Operation) -> Result<DurableApplicationAck> {
        if self.fail_apply.load(Ordering::SeqCst) {
            return Err(RuntimeError::Application(
                "injected durable apply failure".to_string(),
            ));
        }
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
        Ok(*progress)
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
        let mut progress = self.progress.lock().unwrap();
        if committed_lsn > progress.applied_lsn {
            return Err(RuntimeError::Application(
                "commit exceeds applied progress".to_string(),
            ));
        }
        progress.committed_lsn = progress.committed_lsn.max(committed_lsn);
        Ok(*progress)
    }

    async fn copy_operations(&self, from_lsn: i64) -> Result<OperationStream> {
        let operations = self
            .applied
            .lock()
            .unwrap()
            .range(from_lsn..)
            .map(|(_, operation)| Ok(operation.clone()))
            .collect::<Vec<_>>();
        Ok(Box::pin(stream::iter(operations)))
    }

    async fn close(&self) -> Result<()> {
        Ok(())
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
                data: Bytes::from_static(b"closed")
            })
            .await,
        Err(RuntimeError::NotOpen)
    ));
    runtime
        .apply_effect(effect(1, RuntimeEffectAction::Open))
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
        .apply_effect(effect(1, RuntimeEffectAction::Open))
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
    let open = effect(1, RuntimeEffectAction::Open);
    let first = runtime.apply_effect(open.clone()).await.unwrap();
    let duplicate = runtime.apply_effect(open).await.unwrap();
    assert_eq!(duplicate, first);

    let close = effect(2, RuntimeEffectAction::Close);
    runtime.apply_effect(close).await.unwrap();
    assert_eq!(
        runtime
            .apply_effect(effect(1, RuntimeEffectAction::Open))
            .await
            .unwrap(),
        first
    );
    assert!(matches!(
        runtime
            .apply_effect(effect(4, RuntimeEffectAction::Open))
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
        effects: VecDeque::from([effect(1, RuntimeEffectAction::Open)]),
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
        .apply_effect(effect(1, RuntimeEffectAction::Open))
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
        .apply_effect(effect(1, RuntimeEffectAction::Open))
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
            data: Bytes::from_static(b"one"),
        })
        .await
        .unwrap()
        .committed()
        .await
        .unwrap();
    let prepared = source_runtime
        .prepare_copy(OperationId::new("build"), target.clone())
        .await
        .unwrap();
    assert_eq!(prepared.authority.replication_boundary_lsn, 1);
    assert_eq!(prepared.items.len(), 2);
    let live_write = source_runtime
        .begin_write(ClientWrite {
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
        .apply_effect(effect(1, RuntimeEffectAction::Open))
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
            .receive_copy_item(prepared.items[1].clone())
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
    assert_eq!(target_application.applied.lock().unwrap().len(), 1);
    let final_ack = target_runtime
        .receive_copy_item(prepared.items[1].clone())
        .await
        .unwrap();
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
        .receive_copy_item(prepared.items[1].clone())
        .await
        .unwrap();
    assert_eq!(retried_final.durable_lsn, 1);
    source_runtime
        .accept_copy_acknowledgement(retried_final)
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
        .apply_effect(effect(1, RuntimeEffectAction::Open))
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
        .apply_effect(effect(1, RuntimeEffectAction::Open))
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
