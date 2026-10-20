// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual-owner vectors from the pinned native host sources in host.rs.

use super::super::host::{
    ChildClose, ConfigurationStage, Fence, HostError, HostLifecycle, HostOutcome, RemoteReplica,
};
use super::facade::{config, deliver, facade_config, information, until};
use super::*;
use crate::application::{OpenContext, RoleChange};
use crate::protocol::types::{AccessStatus, ReplicaRole};
use crate::replicator::{PrimaryReplicator, ReplicaSetQuorumMode, Replicator};
use crate::traits::StatefulServiceReplica;

struct Application {
    role_gate: Semaphore,
    roles: Mutex<Vec<ReplicaRole>>,
    fail_role: AtomicBool,
    close_gate: Semaphore,
    close_calls: AtomicUsize,
    fail_close: AtomicBool,
    abort_calls: AtomicUsize,
}

impl Application {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            role_gate: Semaphore::new(100),
            roles: Mutex::new(Vec::new()),
            fail_role: AtomicBool::new(false),
            close_gate: Semaphore::new(100),
            close_calls: AtomicUsize::new(0),
            fail_close: AtomicBool::new(false),
            abort_calls: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl StatefulServiceReplica for Application {
    async fn open(self: Arc<Self>, _: OpenContext) -> crate::Result<Arc<dyn Replicator>> {
        Err(RuntimeError::Application(
            "application is already opened by the host caller".into(),
        ))
    }

    async fn change_role(&self, role: ReplicaRole) -> crate::Result<RoleChange> {
        self.roles.lock().unwrap().push(role);
        self.role_gate.acquire().await.unwrap().forget();
        if self.fail_role.load(Ordering::SeqCst) {
            return Err(RuntimeError::Application("application role".into()));
        }
        Ok(RoleChange {
            service_address: Some(format!("service-{role:?}")),
        })
    }

    async fn close(&self) -> crate::Result<()> {
        self.close_calls.fetch_add(1, Ordering::SeqCst);
        self.close_gate.acquire().await.unwrap().forget();
        if self.fail_close.load(Ordering::SeqCst) {
            return Err(RuntimeError::Application("application Close".into()));
        }
        Ok(())
    }

    fn abort(&self) {
        self.abort_calls.fetch_add(1, Ordering::SeqCst);
    }
}

fn host(node: Node, peers: &[(i64, bool)], app: Arc<Application>, minimum: usize) -> HostLifecycle {
    let config = facade_config(&node, peers);
    HostLifecycle::start(node.adapter, config, app, minimum).unwrap()
}

fn remote(id: i64, previous: ReplicaRole, current: ReplicaRole, available: bool) -> RemoteReplica {
    RemoteReplica {
        information: information(id),
        previous,
        current,
        is_up: available,
        ready: available,
    }
}

async fn open(host: &mut HostLifecycle, role: ReplicaRole) {
    host.begin_open(epoch(1), role).unwrap();
    match host.resume().await.unwrap() {
        HostOutcome::Opened {
            replication_address,
            service,
        } => {
            assert!(replication_address.starts_with("replica-"));
            assert_eq!(service.service_address, Some(format!("service-{role:?}")));
        }
        other => panic!("unexpected Open: {other:?}"),
    }
}

async fn primary(host: &mut HostLifecycle) {
    open(host, ReplicaRole::Primary).await;
    host.facade()
        .update_current_replica_set_configuration(config(1, 1, &[]))
        .await
        .unwrap();
}

async fn close(host: &mut HostLifecycle) {
    host.begin_close().unwrap();
    assert!(matches!(
        host.resume().await.unwrap(),
        HostOutcome::Closed(_)
    ));
    host.join().await.unwrap();
}

#[tokio::test]
async fn opening_cancelled_callbacks_and_service_isolation_use_actual_owners() {
    use AccessStatus::{Granted, NotPrimary, ReconfigurationPending};
    let node = lone();
    let old_service = node.service.clone();
    let provider = node.provider.clone();
    provider.epoch_gate.forget_permits(100);
    let app = Application::new();
    app.role_gate.forget_permits(100);
    let mut host = host(node, &[], app.clone(), 1);
    let access = host.access();
    assert_eq!(access.status().unwrap(), (NotPrimary, NotPrimary));
    assert!(matches!(
        old_service.begin_replicate(Bytes::new()),
        Err(AdapterError::WriteAccess(WriteAccess::NotPrimary))
    ));
    host.begin_open(epoch(1), ReplicaRole::Primary).unwrap();
    while provider.epochs.lock().unwrap().is_empty() {
        assert!(host.resume().now_or_never().is_none());
        tokio::task::yield_now().await;
    }
    assert_eq!(
        access.status().unwrap(),
        (ReconfigurationPending, ReconfigurationPending)
    );
    assert!(matches!(
        old_service.begin_replicate(Bytes::new()),
        Err(AdapterError::WriteAccess(
            WriteAccess::ReconfigurationPending
        ))
    ));
    assert!(matches!(host.begin_close(), Err(HostError::Busy)));
    provider.epoch_gate.add_permits(1);
    while app.roles.lock().unwrap().is_empty() {
        assert!(host.resume().now_or_never().is_none());
        tokio::task::yield_now().await;
    }
    assert_eq!(
        access.status().unwrap(),
        (ReconfigurationPending, ReconfigurationPending)
    );
    app.role_gate.add_permits(1);
    host.resume().await.unwrap();
    host.facade()
        .update_current_replica_set_configuration(config(1, 1, &[]))
        .await
        .unwrap();
    assert_eq!(access.status().unwrap(), (Granted, Granted));
    let mut write = old_service
        .begin_replicate(Bytes::from_static(b"prepared"))
        .unwrap();
    write.completion.wait().await.unwrap();
    let mut service = MemoryService {
        visible: BTreeMap::new(),
        commit_gate: Semaphore::new(0),
        fail_commit: false,
    };
    assert!(
        service
            .finish(&mut write, Bytes::from_static(b"prepared"))
            .now_or_never()
            .is_none()
    );
    assert!(service.visible.is_empty());
    assert_eq!(host.facade().current_progress().await.unwrap(), 1);
    assert_eq!(provider.progress.load(Ordering::SeqCst), 0);
    assert_eq!(access.status().unwrap(), (Granted, Granted));
    service.commit_gate.add_permits(1);
    assert_eq!(
        service
            .finish(&mut write, Bytes::from_static(b"prepared"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(service.visible.get(&1).unwrap().as_ref(), b"prepared");
    close(&mut host).await;
    host.join().await.unwrap();
    assert_eq!(access.status().unwrap(), (NotPrimary, NotPrimary));
}

#[tokio::test]
async fn host_availability_majorities_pc_roles_and_minimum_are_not_engine_progress_or_w() {
    use AccessStatus::{Granted, NoWriteQuorum};
    use ReplicaRole::{ActiveSecondary as Secondary, None, Primary};
    let mut host = host(
        lone(),
        &[(2, true), (3, true), (4, true)],
        Application::new(),
        3,
    );
    primary(&mut host).await;
    assert_eq!(host.access().status().unwrap(), (Granted, NoWriteQuorum));
    host.prepare_reconfiguration(
        Primary,
        false,
        ConfigurationStage::Catchup,
        vec![
            remote(2, Secondary, None, false),
            remote(3, Primary, Secondary, true),
            remote(4, None, Secondary, false),
        ],
    )
    .unwrap();
    // CC has majority/minimum; PC's former primary does not count as a PC secondary.
    assert_eq!(host.access().status().unwrap(), (Granted, NoWriteQuorum));
    host.update_availability(&information(2), true, true)
        .unwrap();
    assert_eq!(host.access().status().unwrap(), (Granted, Granted));
    // Actual engine W=1 catch-up can finish while host PC availability is absent.
    host.begin_catchup(ReplicaSetQuorumMode::WriteQuorum)
        .unwrap();
    host.resume().await.unwrap();
    host.update_availability(&information(2), false, false)
        .unwrap();
    assert_eq!(host.access().status().unwrap(), (Granted, NoWriteQuorum));
    host.complete_reconfiguration().unwrap();
    assert_eq!(host.access().status().unwrap(), (Granted, Granted));
    host.update_availability(&information(3), false, true)
        .unwrap();
    host.update_availability(&information(4), true, false)
        .unwrap();
    assert_eq!(host.access().status().unwrap(), (Granted, NoWriteQuorum));
    assert!(matches!(
        host.facade().service().begin_replicate(Bytes::new()),
        Err(AdapterError::WriteAccess(WriteAccess::NoWriteQuorum))
    ));
    host.update_availability(&information(4), true, true)
        .unwrap();
    assert_eq!(host.access().status().unwrap(), (Granted, Granted));
    host.access().fence(Fence::Authority);
    assert_eq!(
        host.access().status().unwrap(),
        (AccessStatus::NotPrimary, AccessStatus::NotPrimary)
    );
    close(&mut host).await;
}

#[tokio::test]
async fn actual_copy_catchup_demotion_and_promotion_select_native_access_stages() {
    use AccessStatus::{Granted, NotPrimary, ReconfigurationPending as TryAgain};
    use ReplicaRole::{ActiveSecondary as Secondary, IdleSecondary as Idle, Primary};
    let bus = Bus::default();
    let registry = MessageRegistry::default();
    let source_app = Application::new();
    let target_app = Application::new();
    let mut source = host(
        Node::new(1, true, false, bus.clone(), registry.clone()),
        &[(2, true)],
        source_app,
        1,
    );
    let mut target = host(
        Node::new(2, true, false, bus.clone(), registry.clone()),
        &[(1, true)],
        target_app.clone(),
        1,
    );
    primary(&mut source).await;
    open(&mut target, Idle).await;
    assert_eq!(target.access().status().unwrap(), (NotPrimary, NotPrimary));
    let primary = source.facade();
    let secondary = target.facade();
    let mut copy = secondary.service().copy_stream().unwrap();
    let mut stream = secondary.service().replication_stream().unwrap();
    let build = primary.build_replica(information(2));
    tokio::pin!(build);
    assert!(build.as_mut().now_or_never().is_none());
    deliver(&bus, &registry, take(&mut copy))
        .await
        .acknowledge()
        .unwrap();
    deliver(&bus, &registry, build.as_mut()).await.unwrap();
    target
        .prepare_reconfiguration(Secondary, false, ConfigurationStage::Activated, vec![])
        .unwrap();
    target.begin_change_role(epoch(1)).unwrap();
    assert!(
        matches!(target.resume().await.unwrap(), HostOutcome::RoleChanged(service)
        if service.service_address == Some("service-ActiveSecondary".into()))
    );
    target.complete_reconfiguration().unwrap();
    assert_eq!(target.access().status().unwrap(), (NotPrimary, NotPrimary));
    primary
        .update_catch_up_replica_set_configuration(
            config(1, 2, &[(2, 0, false)]),
            config(1, 1, &[]),
        )
        .await
        .unwrap();
    source
        .prepare_reconfiguration(
            Secondary,
            true,
            ConfigurationStage::PreWriteStatusRevoke,
            vec![remote(2, Secondary, Primary, true)],
        )
        .unwrap();
    assert!(source.begin_change_role(epoch(2)).is_err());
    assert_eq!(source.access().status().unwrap(), (Granted, Granted));
    let mut write = primary
        .service()
        .begin_replicate(Bytes::from_static(b"one"))
        .unwrap();
    source
        .begin_catchup(ReplicaSetQuorumMode::WriteQuorum)
        .unwrap();
    assert!(source.resume().now_or_never().is_none());
    assert_eq!(source.access().status().unwrap(), (Granted, TryAgain));
    assert!(matches!(
        primary.service().begin_replicate(Bytes::new()),
        Err(AdapterError::WriteAccess(
            WriteAccess::ReconfigurationPending
        ))
    ));
    assert!(source.begin_change_role(epoch(2)).is_err());
    deliver(&bus, &registry, take(&mut stream))
        .await
        .acknowledge()
        .unwrap();
    deliver(&bus, &registry, source.resume()).await.unwrap();
    write.completion.wait().await.unwrap();
    assert_eq!(source.access().status().unwrap(), (NotPrimary, NotPrimary));
    source.begin_change_role(epoch(2)).unwrap();
    source.resume().await.unwrap();
    source.complete_reconfiguration().unwrap();
    assert_eq!(source.access().status().unwrap(), (NotPrimary, NotPrimary));
    target
        .prepare_reconfiguration(
            Primary,
            true,
            ConfigurationStage::Catchup,
            vec![remote(1, Primary, Secondary, true)],
        )
        .unwrap();
    assert_eq!(target.access().status().unwrap(), (TryAgain, TryAgain));
    target_app.role_gate.forget_permits(100);
    target.begin_change_role(epoch(2)).unwrap();
    while target_app.roles.lock().unwrap().len() < 3 {
        assert!(target.resume().now_or_never().is_none());
        tokio::task::yield_now().await;
    }
    assert_eq!(target.access().status().unwrap(), (TryAgain, TryAgain));
    target_app.role_gate.add_permits(1);
    target.resume().await.unwrap();
    secondary
        .update_current_replica_set_configuration(config(2, 1, &[]))
        .await
        .unwrap();
    target
        .begin_catchup(ReplicaSetQuorumMode::WriteQuorum)
        .unwrap();
    target.resume().await.unwrap();
    assert_eq!(target.access().status().unwrap(), (Granted, TryAgain));
    target.complete_reconfiguration().unwrap();
    assert_eq!(target.access().status().unwrap(), (Granted, Granted));
    target
        .update_availability(&information(1), false, true)
        .unwrap();
    assert_eq!(
        target.access().status().unwrap(),
        (Granted, AccessStatus::NoWriteQuorum)
    );
    close(&mut source).await;
    close(&mut target).await;
}

#[tokio::test]
async fn same_primary_reconfiguration_keeps_native_reads_during_actual_held_provider_epoch() {
    let node = lone();
    let provider = node.provider.clone();
    let mut host = host(node, &[], Application::new(), 1);
    primary(&mut host).await;
    host.prepare_reconfiguration(
        ReplicaRole::Primary,
        false,
        ConfigurationStage::Catchup,
        vec![],
    )
    .unwrap();
    provider.epoch_gate.forget_permits(100);
    let facade = host.facade();
    let update = facade.update_epoch(epoch(2));
    tokio::pin!(update);
    assert!(update.as_mut().now_or_never().is_none());
    until(|| provider.epochs.lock().unwrap().len() == 2).await;
    assert_eq!(
        host.access().status().unwrap(),
        (AccessStatus::Granted, AccessStatus::Granted)
    );
    let mut write = facade
        .service()
        .begin_replicate(Bytes::from_static(b"pending-service"))
        .unwrap();
    write.completion.wait().await.unwrap();
    assert_eq!(
        host.access().status().unwrap(),
        (AccessStatus::Granted, AccessStatus::Granted)
    );
    provider.epoch_gate.add_permits(1);
    update.await.unwrap();
    host.begin_catchup(ReplicaSetQuorumMode::WriteQuorum)
        .unwrap();
    host.resume().await.unwrap();
    host.complete_reconfiguration().unwrap();
    close(&mut host).await;
}

#[tokio::test]
async fn failed_application_role_aborts_mismatched_children_without_synthetic_close() {
    let app = Application::new();
    app.fail_role.store(true, Ordering::SeqCst);
    let mut host = host(lone(), &[], app.clone(), 1);
    host.begin_open(epoch(1), ReplicaRole::Primary).unwrap();
    assert!(
        host.resume()
            .await
            .unwrap_err()
            .to_string()
            .contains("application role")
    );
    host.begin_close().unwrap();
    let HostOutcome::Closed(outcome) = host.resume().await.unwrap() else {
        panic!("Close outcome")
    };
    assert!(outcome.role_mismatch);
    assert!(matches!(outcome.replicator, ChildClose::Aborted));
    assert!(matches!(outcome.application, ChildClose::Aborted));
    assert_eq!(app.close_calls.load(Ordering::SeqCst), 0);
    assert_eq!(app.abort_calls.load(Ordering::SeqCst), 1);
    host.join().await.unwrap();
}

#[tokio::test]
async fn outer_close_retains_child_errors_and_cancelled_application_close() {
    let app = Application::new();
    app.close_gate.forget_permits(100);
    app.fail_close.store(true, Ordering::SeqCst);
    let mut host = host(lone(), &[], app.clone(), 1);
    primary(&mut host).await;
    host.begin_close().unwrap();
    while app.close_calls.load(Ordering::SeqCst) == 0 {
        assert!(host.resume().now_or_never().is_none());
        tokio::task::yield_now().await;
    }
    assert_eq!(
        host.access().status().unwrap(),
        (AccessStatus::NotPrimary, AccessStatus::NotPrimary)
    );
    assert!(matches!(host.begin_close(), Err(HostError::Busy)));
    assert!(matches!(host.join().await, Err(HostError::Busy)));
    app.close_gate.add_permits(1);
    let HostOutcome::Closed(outcome) = host.resume().await.unwrap() else {
        panic!("Close outcome")
    };
    assert!(matches!(outcome.replicator, ChildClose::Closed));
    assert!(
        matches!(outcome.application, ChildClose::Failed(ref error) if error.to_string().contains("application Close"))
    );
    assert_eq!(app.close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(app.abort_calls.load(Ordering::SeqCst), 1);
    assert!(host.close_outcome().is_some());
    host.begin_close().unwrap();
    assert!(matches!(
        host.resume().await.unwrap(),
        HostOutcome::Closed(_)
    ));
    assert_eq!(app.close_calls.load(Ordering::SeqCst), 1);
    host.join().await.unwrap();
    host.join().await.unwrap();
}

#[tokio::test]
async fn actual_replicator_close_failure_keeps_diagnostics_after_successful_abort_drain() {
    let node = lone();
    let provider = node.provider.clone();
    let app = Application::new();
    app.fail_close.store(true, Ordering::SeqCst);
    let mut host = host(node, &[], app.clone(), 1);
    primary(&mut host).await;
    provider.fail_epoch.store(true, Ordering::SeqCst);
    assert!(host.facade().update_epoch(epoch(2)).await.is_err());
    host.begin_close().unwrap();
    let HostOutcome::Closed(outcome) = host.resume().await.unwrap() else {
        panic!("Close outcome")
    };
    assert!(!outcome.role_mismatch);
    assert!(matches!(outcome.replicator, ChildClose::Failed(_)));
    assert!(matches!(outcome.application, ChildClose::Failed(_)));
    assert_eq!(app.close_calls.load(Ordering::SeqCst), 1);
    assert_eq!(app.abort_calls.load(Ordering::SeqCst), 1);
    host.join().await.unwrap();
    host.join().await.unwrap();
    assert!(
        host.facade()
            .service()
            .begin_replicate(Bytes::new())
            .is_err()
    );
}

#[tokio::test]
async fn outer_close_does_not_claim_retained_provider_context_drain() {
    let node = Node::new(2, true, false, Bus::default(), MessageRegistry::default());
    let registry = node.registry.clone();
    let provider = node.provider.clone();
    provider.context_gate.forget_permits(100);
    provider.fail_context.store(true, Ordering::SeqCst);
    let app = Application::new();
    let mut host = host(node, &[], app.clone(), 1);
    open(&mut host, ReplicaRole::IdleSecondary).await;
    let message = route(1, 2).envelope(wire::envelope::Body::StartCopy(wire::StartCopy {
        epoch: Some(wire_epoch(epoch(1))),
        replica: 2,
        replication_start: 1,
    }));
    assert_eq!(registry.dispatch(&message).unwrap(), Disposition::Processed);
    until(|| provider.context_calls.load(Ordering::SeqCst) == 1).await;
    host.begin_close().unwrap();
    assert!(matches!(
        host.resume().await.unwrap(),
        HostOutcome::Closed(_)
    ));
    assert_eq!(app.close_calls.load(Ordering::SeqCst), 1);
    assert!(host.join().now_or_never().is_none());
    provider.context_gate.add_permits(1);
    assert!(
        host.join()
            .await
            .unwrap_err()
            .to_string()
            .contains("GetCopyContext")
    );
    assert!(host.join().await.is_err());
    assert_eq!(provider.context_resumed.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn abort_during_retained_callback_cannot_reopen_access_or_publish_late_success() {
    let app = Application::new();
    app.role_gate.forget_permits(100);
    let mut host = host(lone(), &[], app.clone(), 1);
    host.begin_open(epoch(1), ReplicaRole::Primary).unwrap();
    while app.roles.lock().unwrap().is_empty() {
        assert!(host.resume().now_or_never().is_none());
        tokio::task::yield_now().await;
    }
    host.access().fence(Fence::Authority);
    host.abort();
    host.abort();
    assert_eq!(app.abort_calls.load(Ordering::SeqCst), 1);
    app.role_gate.add_permits(1);
    assert!(matches!(host.resume().await, Err(HostError::Aborted)));
    assert_eq!(
        host.access().status().unwrap(),
        (AccessStatus::NotPrimary, AccessStatus::NotPrimary)
    );
    host.join().await.unwrap();
}

#[tokio::test]
async fn demotion_counts_the_remote_current_primary_without_requiring_pc_minimum() {
    let mut host = host(lone(), &[(2, true)], Application::new(), 2);
    primary(&mut host).await;
    assert_eq!(
        host.access().status().unwrap(),
        (AccessStatus::Granted, AccessStatus::NoWriteQuorum)
    );
    host.prepare_reconfiguration(
        ReplicaRole::ActiveSecondary,
        true,
        ConfigurationStage::PreWriteStatusRevoke,
        vec![remote(2, ReplicaRole::None, ReplicaRole::Primary, true)],
    )
    .unwrap();
    assert_eq!(
        host.access().status().unwrap(),
        (AccessStatus::Granted, AccessStatus::Granted)
    );
    host.begin_catchup(ReplicaSetQuorumMode::All).unwrap();
    host.resume().await.unwrap();
    assert_eq!(
        host.access().status().unwrap(),
        (AccessStatus::NotPrimary, AccessStatus::NotPrimary)
    );
    host.begin_change_role(epoch(2)).unwrap();
    host.resume().await.unwrap();
    host.complete_reconfiguration().unwrap();
    close(&mut host).await;
}

#[tokio::test]
async fn abort_after_child_close_retains_late_application_failure_without_outer_success() {
    let app = Application::new();
    let mut host = host(lone(), &[], app.clone(), 1);
    primary(&mut host).await;
    app.close_gate.forget_permits(100);
    app.fail_close.store(true, Ordering::SeqCst);
    host.begin_close().unwrap();
    while app.close_calls.load(Ordering::SeqCst) == 0 {
        assert!(host.resume().now_or_never().is_none());
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        host.close_outcome().unwrap().replicator,
        ChildClose::Closed
    ));
    assert!(matches!(
        host.close_outcome().unwrap().application,
        ChildClose::Pending
    ));
    host.access().fence(Fence::Authority);
    host.abort();
    assert!(matches!(host.join().await, Err(HostError::Busy)));
    app.close_gate.add_permits(1);
    assert!(matches!(host.resume().await, Err(HostError::Aborted)));
    assert!(matches!(host.close_outcome().unwrap().application,
        ChildClose::Failed(error) if error.to_string().contains("application Close")));
    assert_eq!(app.abort_calls.load(Ordering::SeqCst), 1);
    host.join().await.unwrap();
}

#[tokio::test]
async fn actual_worker_panic_denies_access_and_join_reports_the_retained_task_failure() {
    let node = lone();
    let provider = node.provider.clone();
    let mut host = host(node, &[], Application::new(), 1);
    primary(&mut host).await;
    // Enter a real provider callback, rather than a replacement worker fixture.
    provider.panic_progress.store(true, Ordering::SeqCst);
    provider.changed.store(true, Ordering::SeqCst);
    assert!(host.facade().on_data_loss().await.is_err());
    assert!(host.access().status().is_err());
    host.abort();
    assert_eq!(
        host.access().status().unwrap(),
        (AccessStatus::NotPrimary, AccessStatus::NotPrimary)
    );
    assert!(host.join().await.unwrap_err().to_string().contains("join"));
    assert!(host.join().await.is_err());
}

#[tokio::test]
async fn outer_close_after_independent_child_abort_still_closes_the_application() {
    let app = Application::new();
    let mut host = host(lone(), &[], app.clone(), 1);
    primary(&mut host).await;
    host.facade().abort();
    host.facade().join().await.unwrap();
    host.begin_close().unwrap();
    let HostOutcome::Closed(outcome) = host.resume().await.unwrap() else {
        panic!("Close outcome")
    };
    assert!(matches!(outcome.replicator, ChildClose::Aborted));
    assert!(matches!(outcome.application, ChildClose::Closed));
    assert_eq!(app.close_calls.load(Ordering::SeqCst), 1);
    host.join().await.unwrap();
}

#[tokio::test]
async fn invalid_host_membership_and_phase_inputs_fail_before_mutation() {
    let mut host = host(lone(), &[(2, true)], Application::new(), 1);
    primary(&mut host).await;
    let before = host.access().status().unwrap();
    let bad_edge = host.prepare_reconfiguration(
        ReplicaRole::Primary,
        true,
        ConfigurationStage::Catchup,
        vec![],
    );
    assert!(bad_edge.is_err());
    let mut stale = remote(2, ReplicaRole::None, ReplicaRole::ActiveSecondary, true);
    stale.information.process_session_id = crate::protocol::types::ProcessSessionId::new("stale");
    assert!(
        host.prepare_reconfiguration(
            ReplicaRole::Primary,
            false,
            ConfigurationStage::Catchup,
            vec![stale]
        )
        .is_err()
    );
    let peer = remote(2, ReplicaRole::None, ReplicaRole::ActiveSecondary, true);
    assert!(
        host.prepare_reconfiguration(
            ReplicaRole::Primary,
            false,
            ConfigurationStage::Catchup,
            vec![peer.clone(), peer]
        )
        .is_err()
    );
    assert!(
        host.update_availability(&information(2), true, true)
            .is_err()
    );
    assert!(host.begin_catchup(ReplicaSetQuorumMode::All).is_err());
    assert!(host.complete_reconfiguration().is_err());
    assert_eq!(host.access().status().unwrap(), before);
    close(&mut host).await;
}
