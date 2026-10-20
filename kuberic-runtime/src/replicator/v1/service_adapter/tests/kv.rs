// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual private V1 owners with the real example KV storage and commit worker.

use super::facade::{config, facade_config, until};
use super::*;
use crate::application::{OpenContext, RoleChange};
use crate::host::v1_lifecycle::{Fence, HostLifecycle, HostOutcome, PartitionAccess};
use crate::protocol::types::{AccessStatus, ReplicaRole};
use crate::replicator::{PrimaryReplicator, Replicator};
use crate::traits::StatefulServiceReplica;
use kvstore2::v1_test_bridge::{Assignment, Committer, Write};
use std::io;

fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn application(error: io::Error) -> RuntimeError {
    RuntimeError::Application(error.to_string())
}

struct KvProvider(Arc<Committer>);

#[async_trait]
impl StateProvider for KvProvider {
    async fn update_epoch(&self, epoch: Epoch, _: Lsn) -> crate::Result<()> {
        self.0
            .update_epoch(epoch.data_loss_number, epoch.configuration_number)
            .map_err(application)
    }

    async fn last_committed_lsn(&self) -> crate::Result<Lsn> {
        self.0.progress().map_err(application)
    }

    async fn get_copy_context(&self) -> crate::Result<OperationDataStream> {
        Ok(Box::pin(stream::empty()))
    }

    async fn get_copy_state(
        &self,
        up_to: Lsn,
        mut context: OperationDataStream,
    ) -> crate::Result<OperationDataStream> {
        if let Some(item) = context.next().await {
            item?;
            return Err(RuntimeError::Application(
                "KV does not accept copy context data".into(),
            ));
        }
        let snapshot = self.0.snapshot(up_to).await.map_err(application)?;
        Ok(Box::pin(stream::iter([Ok(snapshot)])))
    }

    async fn on_data_loss(&self) -> crate::Result<bool> {
        Ok(false)
    }
}

struct KvApplication(Arc<Committer>);

#[async_trait]
impl StatefulServiceReplica for KvApplication {
    async fn open(self: Arc<Self>, _: OpenContext) -> crate::Result<Arc<dyn Replicator>> {
        Err(RuntimeError::Application(
            "private caller already opened KV".into(),
        ))
    }

    async fn change_role(&self, role: ReplicaRole) -> crate::Result<RoleChange> {
        match role {
            ReplicaRole::Primary => self.0.promote().map_err(application)?,
            ReplicaRole::IdleSecondary => self.0.begin_copy().map_err(application)?,
            ReplicaRole::ActiveSecondary | ReplicaRole::None => {}
        }
        Ok(RoleChange {
            service_address: Some("kv-private".into()),
        })
    }

    async fn close(&self) -> crate::Result<()> {
        self.0.close().await.map_err(application)
    }

    fn abort(&self) {
        self.0.abort();
    }
}

fn store(path: &std::path::Path) -> Arc<Committer> {
    Arc::new(
        Committer::open(
            path,
            Arc::new(|reason| panic!("unexpected KV fault: {reason}")),
        )
        .unwrap(),
    )
}

fn assign(service: &ServiceHandle, data: Bytes) -> io::Result<Assignment> {
    let mut operation = service.begin_replicate(data).map_err(io_error)?;
    Ok(Assignment {
        lsn: operation.lsn,
        completion: Box::pin(async move { operation.completion.wait().await.map_err(io_error) }),
    })
}

fn put(kv: &Committer, service: &ServiceHandle, value: &str) -> Write {
    kv.begin_put("k".into(), value.into(), |data| assign(service, data))
        .unwrap()
}

async fn primary(kv: Arc<Committer>, bus: Bus, registry: MessageRegistry) -> Node<KvProvider> {
    let mut node = Node::with_provider(
        1,
        true,
        false,
        bus,
        registry,
        Arc::new(KvProvider(kv.clone())),
    );
    node.primary().await;
    kv.promote().unwrap();
    node
}

async fn copied(source: &mut Node<KvProvider>, kv: Arc<Committer>) -> Node<KvProvider> {
    copied_with_eos(source, kv, false).await
}

async fn copied_with_eos(
    source: &mut Node<KvProvider>,
    kv: Arc<Committer>,
    eos: bool,
) -> Node<KvProvider> {
    let mut target = Node::with_provider(
        2,
        true,
        eos,
        source.bus.clone(),
        source.registry.clone(),
        Arc::new(KvProvider(kv.clone())),
    );
    target.secondary().await;
    kv.begin_copy().unwrap();
    let mut copy = target.service.copy_stream().unwrap();
    let build = source.build(2, true);
    source.bus.deliver(&source.registry);
    assert!(target.adapter.drive().now_or_never().is_none());
    source.bus.deliver(&source.registry);
    assert!(source.adapter.drive().now_or_never().is_none());
    source.bus.deliver(&source.registry);
    let snapshot = take(&mut copy).await;
    assert!(build.wait().now_or_never().is_none());
    kv.install_copy(snapshot.data().clone()).unwrap();
    snapshot.acknowledge().unwrap();
    if eos {
        let end = take(&mut copy).await;
        assert!(end.is_end_of_stream());
        end.acknowledge().unwrap();
    }
    assert!(copy.next().await.is_none());
    kv.finish_copy().unwrap();
    source.bus.deliver(&source.registry);
    assert!(source.adapter.drive().now_or_never().is_none());
    build.wait().await.unwrap();
    target
        .adapter
        .change_role(epoch(1), Role::ActiveSecondary)
        .await
        .unwrap();
    source
        .adapter
        .update_catchup_configuration(None, configuration(1, 2, &[(2, kv.progress().unwrap())]))
        .unwrap();
    target
}

async fn close(mut source: Node<KvProvider>, mut target: Node<KvProvider>) {
    source.adapter.close(Duration::ZERO).await.unwrap();
    target.adapter.close(Duration::ZERO).await.unwrap();
    source.adapter.drain_retired_builds().await.unwrap();
    source.provider.0.close().await.unwrap();
    target.provider.0.close().await.unwrap();
}

#[tokio::test]
async fn real_copy_ack_quorum_and_local_commit_are_distinct_and_client_drop_does_not_rollback() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let source_kv = store(a.path());
    let target_kv = store(b.path());
    let mut source = primary(
        source_kv.clone(),
        Bus::default(),
        MessageRegistry::default(),
    )
    .await;
    let target = copied(&mut source, target_kv.clone()).await;
    let mut replication = target.service.replication_stream().unwrap();
    let mut gate = source_kv.pause_next_commit();
    let mut first = put(&source_kv, &source.service, "a");
    let second = put(&source_kv, &source.service, "b");
    assert_eq!((first.lsn, second.lsn), (1, 2));
    assert!(first.wait().now_or_never().is_none());
    assert_eq!(source_kv.get("k", || Ok(())).unwrap(), None);
    assert_eq!(source.adapter.current_progress().await.unwrap(), 0);
    source.bus.deliver(&source.registry);
    let one = take(&mut replication).await;
    let two = take(&mut replication).await;
    assert_eq!(target_kv.progress().unwrap(), 0);
    target_kv
        .apply_secondary(one.lsn(), one.data().clone())
        .unwrap();
    assert_eq!(target_kv.progress().unwrap(), 1);
    source.bus.deliver(&source.registry);
    assert_eq!(source.adapter.current_progress().await.unwrap(), 0);
    assert!(first.wait().now_or_never().is_none());
    one.acknowledge().unwrap();
    target_kv
        .apply_secondary(two.lsn(), two.data().clone())
        .unwrap();
    two.acknowledge().unwrap();
    source.bus.deliver(&source.registry);
    gate.entered().await;
    assert_eq!(source.adapter.current_progress().await.unwrap(), 2);
    assert_eq!(source_kv.progress().unwrap(), 0);
    assert_eq!(source_kv.get("k", || Ok(())).unwrap(), None);
    assert!(first.wait().now_or_never().is_none());
    assert!(source_kv.snapshot(2).now_or_never().is_none());
    assert!(source_kv.demote().is_err());
    drop(second);
    gate.release();
    assert_eq!(first.wait().await.unwrap(), 1);
    assert_eq!(first.wait().await.unwrap(), 1);
    until(|| source_kv.progress().unwrap() == 2).await;
    assert_eq!(source_kv.get("k", || Ok(())).unwrap().as_deref(), Some("b"));
    assert_eq!(target_kv.get("k", || Ok(())).unwrap().as_deref(), Some("b"));
    close(source, target).await;
    let reopened = store(a.path());
    assert_eq!(reopened.progress().unwrap(), 2);
    assert_eq!(reopened.get("k", || Ok(())).unwrap().as_deref(), Some("b"));
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn native_host_granted_reads_remain_isolated_and_local_failure_fences_without_retracting_quorum()
 {
    let directory = tempfile::tempdir().unwrap();
    let faults = Arc::new(Mutex::new(Vec::new()));
    let reported = faults.clone();
    let access_slot: Arc<Mutex<Option<PartitionAccess>>> = Arc::new(Mutex::new(None));
    let fence = access_slot.clone();
    let kv = Arc::new(
        Committer::open(
            directory.path(),
            Arc::new(move |reason| {
                reported.lock().unwrap().push(reason);
                if let Some(access) = fence.lock().unwrap().as_ref() {
                    access.fence(Fence::Fault);
                }
            }),
        )
        .unwrap(),
    );
    let node = Node::with_provider(
        1,
        true,
        false,
        Bus::default(),
        MessageRegistry::default(),
        Arc::new(KvProvider(kv.clone())),
    );
    let service = node.service.clone();
    let wiring = facade_config(&node, &[]);
    let mut host = HostLifecycle::start(
        FacadeSetup::new(node.adapter, wiring),
        Arc::new(KvApplication(kv.clone())),
        1,
    )
    .unwrap();
    let access = host.access();
    *access_slot.lock().unwrap() = Some(access.clone());
    host.begin_open(epoch(1), ReplicaRole::Primary).unwrap();
    assert!(matches!(
        host.resume().await.unwrap(),
        HostOutcome::Opened { .. }
    ));
    host.facade()
        .update_current_replica_set_configuration(config(1, 1, &[]))
        .await
        .unwrap();
    let read = || {
        if access.status().map_err(io_error)?.0 == AccessStatus::Granted {
            Ok(())
        } else {
            Err(io::Error::other("native host denied read"))
        }
    };
    assert_eq!(
        access.status().unwrap(),
        (AccessStatus::Granted, AccessStatus::Granted)
    );
    let mut gate = kv.pause_next_commit();
    let mut first = put(&kv, &service, "a");
    gate.entered().await;
    assert_eq!(host.facade().current_progress().await.unwrap(), 1);
    assert_eq!(
        access.status().unwrap(),
        (AccessStatus::Granted, AccessStatus::Granted)
    );
    assert_eq!(kv.get("k", read).unwrap(), None);
    gate.release();
    first.wait().await.unwrap();
    assert_eq!(kv.get("k", read).unwrap().as_deref(), Some("a"));
    let mut gate = kv.pause_next_commit();
    let mut failed = put(&kv, &service, "b");
    let mut later = put(&kv, &service, "c");
    gate.entered().await;
    assert_eq!(host.facade().current_progress().await.unwrap(), 3);
    // Fail the existing atomic persistence primitive using a named test-local
    // staging path. No permissions/root behavior or synthetic store is needed.
    std::fs::create_dir(directory.path().join("state.json.tmp")).unwrap();
    gate.release();
    assert!(failed.wait().await.is_err());
    assert!(later.wait().await.is_err());
    assert_eq!(faults.lock().unwrap().len(), 1);
    assert_ne!(access.status().unwrap().0, AccessStatus::Granted);
    assert!(kv.get("k", || Ok(())).is_err());
    assert!(
        kv.begin_put("x".into(), "y".into(), |data| assign(&service, data))
            .is_err()
    );
    assert_eq!(host.facade().current_progress().await.unwrap(), 3);
    host.begin_close().unwrap();
    let HostOutcome::Closed(outcome) = host.resume().await.unwrap() else {
        panic!("outer Close");
    };
    assert!(matches!(
        outcome.application,
        crate::host::v1_lifecycle::ChildClose::Failed(_)
    ));
    host.join().await.unwrap();
    assert!(kv.close().await.is_err());
    std::fs::remove_dir(directory.path().join("state.json.tmp")).unwrap();
    let reopened = store(directory.path());
    assert_eq!(reopened.progress().unwrap(), 1);
    assert_eq!(reopened.get("k", || Ok(())).unwrap().as_deref(), Some("a"));
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn real_restart_copy_promotion_and_later_assignment_preserve_service_state() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let source_kv = store(a.path());
    let mut source = primary(
        source_kv.clone(),
        Bus::default(),
        MessageRegistry::default(),
    )
    .await;
    put(&source_kv, &source.service, "a").wait().await.unwrap();
    source.adapter.close(Duration::ZERO).await.unwrap();
    source_kv.close().await.unwrap();
    drop(source);
    drop(source_kv);
    let source_kv = store(a.path());
    let target_kv = store(b.path());
    let mut source = primary(
        source_kv.clone(),
        Bus::default(),
        MessageRegistry::default(),
    )
    .await;
    assert_eq!(source.adapter.current_progress().await.unwrap(), 1);
    let mut target = copied(&mut source, target_kv.clone()).await;
    assert_eq!(target_kv.get("k", || Ok(())).unwrap().as_deref(), Some("a"));
    let mut replication = target.service.replication_stream().unwrap();
    let mut write = put(&source_kv, &source.service, "b");
    assert_eq!(write.lsn, 2);
    source.bus.deliver(&source.registry);
    let operation = take(&mut replication).await;
    target_kv
        .apply_secondary(operation.lsn(), operation.data().clone())
        .unwrap();
    operation.acknowledge().unwrap();
    source.bus.deliver(&source.registry);
    write.wait().await.unwrap();
    source
        .adapter
        .update_current_configuration(configuration(1, 2, &[(2, 2)]))
        .unwrap();
    target
        .adapter
        .change_role(epoch(2), Role::Primary)
        .await
        .unwrap();
    target_kv.promote().unwrap();
    target
        .adapter
        .update_current_configuration(configuration(2, 1, &[]))
        .unwrap();
    assert_eq!(target.adapter.current_progress().await.unwrap(), 2);
    assert_eq!(
        put(&target_kv, &target.service, "c").wait().await.unwrap(),
        3
    );
    assert_eq!(target_kv.get("k", || Ok(())).unwrap().as_deref(), Some("c"));
    close(source, target).await;
    let reopened = store(b.path());
    assert_eq!(reopened.progress().unwrap(), 3);
    assert_eq!(reopened.get("k", || Ok(())).unwrap().as_deref(), Some("c"));
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn cancelled_service_close_retains_work_and_abort_suppresses_late_success() {
    for abort in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let faults = Arc::new(Mutex::new(Vec::new()));
        let reported = faults.clone();
        let kv = Arc::new(
            Committer::open(
                directory.path(),
                Arc::new(move |reason| {
                    reported.lock().unwrap().push(reason);
                }),
            )
            .unwrap(),
        );
        let mut source = primary(kv.clone(), Bus::default(), MessageRegistry::default()).await;
        let mut gate = kv.pause_next_commit();
        let mut write = put(&kv, &source.service, "a");
        gate.entered().await;
        assert!(kv.close().now_or_never().is_none());
        assert!(
            kv.begin_put("k".into(), "b".into(), |data| assign(&source.service, data))
                .is_err()
        );
        if abort {
            kv.abort();
            assert!(write.wait().await.is_err());
            assert!(kv.close().await.is_err());
            source.adapter.abort().unwrap();
            source.adapter.drain_retired_builds().await.unwrap();
            assert_eq!(faults.lock().unwrap().len(), 1);
        } else {
            gate.release();
            assert_eq!(write.wait().await.unwrap(), 1);
            kv.close().await.unwrap();
            kv.close().await.unwrap();
            source.adapter.close(Duration::ZERO).await.unwrap();
        }
        drop(source);
        drop(kv);
        let reopened = store(directory.path());
        assert_eq!(reopened.progress().unwrap(), if abort { 0 } else { 1 });
        reopened.close().await.unwrap();
    }
}

#[tokio::test]
async fn actual_copy_provider_waits_for_service_commit_instead_of_snapshotting_prepares() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let source_kv = store(a.path());
    let target_kv = store(b.path());
    let mut source = primary(
        source_kv.clone(),
        Bus::default(),
        MessageRegistry::default(),
    )
    .await;
    let mut gate = source_kv.pause_next_commit();
    let mut write = put(&source_kv, &source.service, "a");
    gate.entered().await;
    assert_eq!(source.adapter.current_progress().await.unwrap(), 1);
    let mut target = Node::with_provider(
        2,
        true,
        false,
        source.bus.clone(),
        source.registry.clone(),
        Arc::new(KvProvider(target_kv.clone())),
    );
    target.secondary().await;
    target_kv.begin_copy().unwrap();
    let mut copy = target.service.copy_stream().unwrap();
    let build = source.build(2, true);
    source.bus.deliver(&source.registry);
    assert!(target.adapter.drive().now_or_never().is_none());
    source.bus.deliver(&source.registry);
    assert!(source.adapter.drive().now_or_never().is_none());
    source.bus.deliver(&source.registry);
    assert!(copy.next().now_or_never().is_none());
    assert!(build.wait().now_or_never().is_none());
    gate.release();
    write.wait().await.unwrap();
    assert!(source.adapter.drive().now_or_never().is_none());
    source.bus.deliver(&source.registry);
    let snapshot = take(&mut copy).await;
    target_kv.install_copy(snapshot.data().clone()).unwrap();
    assert_eq!(target_kv.progress().unwrap(), 1);
    assert_eq!(target_kv.get("k", || Ok(())).unwrap().as_deref(), Some("a"));
    snapshot.acknowledge().unwrap();
    assert!(copy.next().await.is_none());
    target_kv.finish_copy().unwrap();
    source.bus.deliver(&source.registry);
    assert!(source.adapter.drive().now_or_never().is_none());
    build.wait().await.unwrap();
    close(source, target).await;
}

#[tokio::test]
async fn secondary_persistence_failure_reports_stream_fault_without_service_ack_or_client_success()
{
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let source_faults = Arc::new(Mutex::new(Vec::new()));
    let reported = source_faults.clone();
    let source_kv = Arc::new(
        Committer::open(
            a.path(),
            Arc::new(move |reason| {
                reported.lock().unwrap().push(reason);
            }),
        )
        .unwrap(),
    );
    let target_faults = Arc::new(Mutex::new(Vec::new()));
    let reported = target_faults.clone();
    let target_kv = Arc::new(
        Committer::open(
            b.path(),
            Arc::new(move |reason| {
                reported.lock().unwrap().push(reason);
            }),
        )
        .unwrap(),
    );
    let mut source = primary(
        source_kv.clone(),
        Bus::default(),
        MessageRegistry::default(),
    )
    .await;
    let mut target = copied_with_eos(&mut source, target_kv.clone(), true).await;
    let mut replication = target.service.replication_stream().unwrap();
    let mut write = put(&source_kv, &source.service, "a");
    source.bus.deliver(&source.registry);
    let operation = take(&mut replication).await;
    std::fs::create_dir(b.path().join("state.json.tmp")).unwrap();
    let error = target_kv
        .apply_secondary(operation.lsn(), operation.data().clone())
        .unwrap_err();
    replication.report_fault(error.to_string()).unwrap();
    drop(operation);
    source.bus.deliver(&source.registry);
    assert_eq!(target_faults.lock().unwrap().len(), 1);
    assert!(matches!(
        target.adapter.current_progress().await,
        Err(AdapterError::Secondary(SecondaryError::Fault(_)))
    ));
    assert_eq!(source.adapter.current_progress().await.unwrap(), 0);
    assert_eq!(source_kv.progress().unwrap(), 0);
    assert!(write.wait().now_or_never().is_none());
    target.adapter.abort().unwrap();
    source.adapter.abort().unwrap();
    assert!(write.wait().await.is_err());
    assert!(source_kv.close().await.is_err());
    assert!(target_kv.close().await.is_err());
    assert_eq!(source_faults.lock().unwrap().len(), 1);
    source.adapter.drain_retired_builds().await.unwrap();
    target.adapter.drain_retired_builds().await.unwrap();
    std::fs::remove_dir(b.path().join("state.json.tmp")).unwrap();
    let reopened = store(b.path());
    assert_eq!(reopened.progress().unwrap(), 0);
    reopened.close().await.unwrap();
}
