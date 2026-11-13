// Copyright (c) Microsoft Corporation. All rights reserved.
// Licensed under the MIT License. See LICENSES/service-fabric-MIT.txt.
//
// Actual V1 owners with the real SQLite frame log, connection and WAL staging VFS.

use super::facade::{config, facade_config};
use super::*;
use crate::application::{OpenContext, RoleChange};
use crate::host::v1_lifecycle::{Fence, HostLifecycle, HostOutcome, PartitionAccess};
use crate::protocol::types::{AccessStatus, ReplicaRole};
use crate::replicator::{PrimaryReplicator, Replicator};
use crate::traits::StatefulServiceReplica;
use rusqlite::types::Value;
use sqlite_replicated::v1_test_bridge::{Admission, Assignment, Committer, Write};
use std::io;

fn io_error(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

fn application(error: io::Error) -> RuntimeError {
    RuntimeError::Application(error.to_string())
}

struct SqliteProvider(Arc<Committer>);

#[async_trait]
impl StateProvider for SqliteProvider {
    async fn update_epoch(&self, _: Epoch, previous: Lsn) -> crate::Result<()> {
        if previous < 0 {
            return Err(RuntimeError::Application(
                "negative SQLite epoch boundary".into(),
            ));
        }
        self.0.progress().map(|_| ()).map_err(application)
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
                "SQLite does not accept copy context".into(),
            ));
        }
        let snapshot = self.0.snapshot(up_to).await.map_err(application)?;
        Ok(Box::pin(stream::iter([Ok(snapshot)])))
    }

    async fn on_data_loss(&self) -> crate::Result<bool> {
        Ok(false)
    }
}

struct SqliteApplication {
    sql: Arc<Committer>,
    admission: Admission,
}

#[async_trait]
impl StatefulServiceReplica for SqliteApplication {
    async fn open(self: Arc<Self>, _: OpenContext) -> crate::Result<Arc<dyn Replicator>> {
        Err(RuntimeError::Application(
            "private caller already opened SQLite".into(),
        ))
    }

    async fn change_role(&self, role: ReplicaRole) -> crate::Result<RoleChange> {
        match role {
            ReplicaRole::Primary => self
                .sql
                .promote(self.admission.clone())
                .map_err(application)?,
            ReplicaRole::IdleSecondary => self.sql.begin_copy().map_err(application)?,
            ReplicaRole::ActiveSecondary | ReplicaRole::None => {}
        }
        Ok(RoleChange {
            service_address: Some("sqlite-private".into()),
        })
    }

    async fn close(&self) -> crate::Result<()> {
        self.sql.close().await.map_err(application)
    }
    fn abort(&self) {
        self.sql.abort();
    }
}

fn store(path: &std::path::Path) -> Arc<Committer> {
    Arc::new(
        Committer::open(
            path.into(),
            Arc::new(|_| Ok(())),
            Arc::new(|reason| panic!("unexpected SQLite fault: {reason}")),
        )
        .unwrap(),
    )
}

fn admission(service: &ServiceHandle) -> Admission {
    let service = service.clone();
    Arc::new(move |data| {
        let mut operation = service.begin_replicate(data).map_err(io_error)?;
        Ok(Assignment {
            lsn: operation.lsn,
            completion: Box::pin(
                async move { operation.completion.wait().await.map_err(io_error) },
            ),
        })
    })
}

fn execute(sql: &Committer, statement: &str) -> Write {
    sql.begin_execute(vec![statement.into()]).unwrap()
}

fn node(
    id: i64,
    eos: bool,
    bus: Bus,
    registry: MessageRegistry,
    sql: Arc<Committer>,
) -> Node<SqliteProvider> {
    Node::with_provider_limits(
        id,
        true,
        eos,
        bus,
        registry,
        Arc::new(SqliteProvider(sql)),
        BufferLimits {
            max_segments: 4,
            max_payload_bytes: 1024 * 1024,
        },
    )
}

async fn primary(sql: Arc<Committer>, bus: Bus, registry: MessageRegistry) -> Node<SqliteProvider> {
    let mut node = node(1, false, bus, registry, sql.clone());
    node.primary().await;
    sql.promote(admission(&node.service)).unwrap();
    node
}

async fn copied(
    source: &mut Node<SqliteProvider>,
    sql: Arc<Committer>,
    eos: bool,
) -> Node<SqliteProvider> {
    let mut target = node(
        2,
        eos,
        source.bus.clone(),
        source.registry.clone(),
        sql.clone(),
    );
    target.secondary().await;
    sql.begin_copy().unwrap();
    let mut copy = target.service.copy_stream().unwrap();
    let build = source.build(2, true);
    source.bus.deliver(&source.registry);
    assert!(target.adapter.drive().now_or_never().is_none());
    source.bus.deliver(&source.registry);
    assert!(source.adapter.drive().now_or_never().is_none());
    source.bus.deliver(&source.registry);
    let snapshot = take(&mut copy).await;
    assert!(build.wait().now_or_never().is_none());
    sql.install_copy(snapshot.data().clone()).unwrap();
    assert!(sql.promote(admission(&target.service)).is_err());
    snapshot.acknowledge().unwrap();
    if eos {
        let end = take(&mut copy).await;
        assert!(end.is_end_of_stream());
        end.acknowledge().unwrap();
    }
    assert!(copy.next().await.is_none());
    sql.finish_copy().unwrap();
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
        .update_catchup_configuration(None, configuration(1, 2, &[(2, sql.progress().unwrap())]))
        .unwrap();
    target
}

async fn close(mut source: Node<SqliteProvider>, mut target: Node<SqliteProvider>) {
    source.adapter.close(Duration::ZERO).await.unwrap();
    target.adapter.close(Duration::ZERO).await.unwrap();
    source.adapter.drain_retired_builds().await.unwrap();
    source.provider.0.close().await.unwrap();
    target.provider.0.close().await.unwrap();
}

#[tokio::test]
async fn actual_assignment_durable_ack_quorum_local_commit_and_wal_publication_are_distinct() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let source_sql = store(a.path());
    let target_sql = store(b.path());
    let mut source = primary(
        source_sql.clone(),
        Bus::default(),
        MessageRegistry::default(),
    )
    .await;
    execute(&source_sql, "CREATE TABLE t (v INTEGER)")
        .wait()
        .await
        .unwrap();
    let target = copied(&mut source, target_sql.clone(), false).await;
    let mut replication = target.service.replication_stream().unwrap();
    source_sql.before_commit().arm();
    source_sql.before_publication().arm();
    let mut write = execute(&source_sql, "INSERT INTO t VALUES (7)");
    assert_eq!(write.assigned().await.unwrap(), 2);
    assert_eq!(source.adapter.current_progress().await.unwrap(), 1);
    assert!(write.wait().now_or_never().is_none());
    source.bus.deliver(&source.registry);
    let operation = take(&mut replication).await;
    assert_eq!(target_sql.progress().unwrap(), 1);
    target_sql
        .apply_secondary(operation.lsn(), operation.data().clone())
        .unwrap();
    assert_eq!(target_sql.progress().unwrap(), 2);
    assert_eq!(
        target_sql.query("SELECT * FROM t").unwrap(),
        vec![vec![Value::Integer(7)]]
    );
    source.bus.deliver(&source.registry);
    assert_eq!(source.adapter.current_progress().await.unwrap(), 1);
    assert!(write.wait().now_or_never().is_none());
    operation.acknowledge().unwrap();
    source.bus.deliver(&source.registry);
    source_sql.before_commit().wait_entered().await;
    assert_eq!(source.adapter.current_progress().await.unwrap(), 2);
    assert_eq!(source_sql.progress().unwrap(), 1);
    assert!(source_sql.snapshot(2).now_or_never().is_none());
    assert_eq!(
        source_sql.query("SELECT * FROM t").unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    assert!(source_sql.demote().is_err());
    source_sql.before_commit().release();
    source_sql.before_publication().wait_entered().await;
    assert_eq!(source_sql.progress().unwrap(), 2);
    assert!(source_sql.snapshot(2).await.is_ok());
    assert!(write.wait().now_or_never().is_none());
    source_sql.before_publication().release();
    assert_eq!(write.wait().await.unwrap(), 2);
    assert_eq!(
        source_sql.query("SELECT * FROM t").unwrap(),
        vec![vec![Value::Integer(7)]]
    );
    close(source, target).await;
}

#[tokio::test]
async fn real_file_restart_copy_and_native_promotion_preserve_sql_and_later_assignment() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let sql = store(a.path());
    let mut source = primary(sql.clone(), Bus::default(), MessageRegistry::default()).await;
    sql.begin_execute(vec![
        "CREATE TABLE t (v INTEGER)".into(),
        "INSERT INTO t VALUES (7)".into(),
    ])
    .unwrap()
    .wait()
    .await
    .unwrap();
    source.adapter.close(Duration::ZERO).await.unwrap();
    sql.close().await.unwrap();
    drop(source);
    drop(sql);
    let sql = store(a.path());
    let target_sql = store(b.path());
    let mut source = primary(sql.clone(), Bus::default(), MessageRegistry::default()).await;
    assert_eq!(source.adapter.current_progress().await.unwrap(), 1);
    let mut target = copied(&mut source, target_sql.clone(), false).await;
    let mut replication = target.service.replication_stream().unwrap();
    let mut write = execute(&sql, "INSERT INTO t VALUES (8)");
    assert_eq!(write.assigned().await.unwrap(), 2);
    source.bus.deliver(&source.registry);
    let operation = take(&mut replication).await;
    target_sql
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
    target_sql.promote(admission(&target.service)).unwrap();
    target
        .adapter
        .update_current_configuration(configuration(2, 1, &[]))
        .unwrap();
    let mut later = execute(&target_sql, "INSERT INTO t VALUES (9)");
    assert_eq!(later.assigned().await.unwrap(), 3);
    assert_eq!(later.wait().await.unwrap(), 3);
    close(source, target).await;
    let reopened = store(b.path());
    reopened.promote(Arc::new(|_| unreachable!())).unwrap();
    assert_eq!(reopened.progress().unwrap(), 3);
    assert_eq!(
        reopened.query("SELECT v FROM t ORDER BY v").unwrap(),
        vec![
            vec![Value::Integer(7)],
            vec![Value::Integer(8)],
            vec![Value::Integer(9)]
        ]
    );
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn actual_copy_provider_waits_for_local_sql_commit() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let sql = store(a.path());
    let target_sql = store(b.path());
    let mut source = primary(sql.clone(), Bus::default(), MessageRegistry::default()).await;
    sql.before_commit().arm();
    let mut write = execute(&sql, "CREATE TABLE t (v INTEGER)");
    sql.before_commit().wait_entered().await;
    let mut target = node(
        2,
        false,
        source.bus.clone(),
        source.registry.clone(),
        target_sql.clone(),
    );
    target.secondary().await;
    target_sql.begin_copy().unwrap();
    let mut copy = target.service.copy_stream().unwrap();
    let build = source.build(2, true);
    source.bus.deliver(&source.registry);
    assert!(target.adapter.drive().now_or_never().is_none());
    source.bus.deliver(&source.registry);
    assert!(source.adapter.drive().now_or_never().is_none());
    source.bus.deliver(&source.registry);
    assert!(copy.next().now_or_never().is_none());
    assert!(build.wait().now_or_never().is_none());
    sql.before_commit().release();
    write.wait().await.unwrap();
    assert!(source.adapter.drive().now_or_never().is_none());
    source.bus.deliver(&source.registry);
    let snapshot = take(&mut copy).await;
    target_sql.install_copy(snapshot.data().clone()).unwrap();
    assert_eq!(target_sql.progress().unwrap(), 1);
    snapshot.acknowledge().unwrap();
    assert!(copy.next().await.is_none());
    target_sql.finish_copy().unwrap();
    source.bus.deliver(&source.registry);
    assert!(source.adapter.drive().now_or_never().is_none());
    build.wait().await.unwrap();
    assert!(target_sql.query("SELECT * FROM t").unwrap().is_empty());
    close(source, target).await;
}

#[tokio::test]
async fn native_host_grants_do_not_publish_sql_early_and_local_failure_fences_without_retracting_quorum()
 {
    let root = tempfile::tempdir().unwrap();
    let access_slot: Arc<Mutex<Option<PartitionAccess>>> = Arc::new(Mutex::new(None));
    let faults = Arc::new(Mutex::new(Vec::new()));
    let observed = access_slot.clone();
    let fenced = access_slot.clone();
    let reported = faults.clone();
    let sql = Arc::new(
        Committer::open(
            root.path().into(),
            Arc::new(move |write| {
                let access = observed
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| io::Error::other("native host not open"))?;
                let (read, writable) = access.status().map_err(io_error)?;
                if (if write { writable } else { read }) != AccessStatus::Granted {
                    return Err(io::Error::other("native host denied SQL"));
                }
                Ok(())
            }),
            Arc::new(move |reason| {
                reported.lock().unwrap().push(reason);
                if let Some(access) = fenced.lock().unwrap().as_ref() {
                    access.fence(Fence::Fault);
                }
            }),
        )
        .unwrap(),
    );
    let node = node(
        1,
        false,
        Bus::default(),
        MessageRegistry::default(),
        sql.clone(),
    );
    let wiring = facade_config(&node, &[]);
    let app = Arc::new(SqliteApplication {
        sql: sql.clone(),
        admission: admission(&node.service),
    });
    let mut host = HostLifecycle::start(FacadeSetup::new(node.adapter, wiring), app, 1).unwrap();
    let access = host.access();
    *access_slot.lock().unwrap() = Some(access.clone());
    assert!(sql.begin_execute(vec![]).is_err());
    host.begin_open(epoch(1), ReplicaRole::Primary).unwrap();
    assert!(matches!(
        host.resume().await.unwrap(),
        HostOutcome::Opened { .. }
    ));
    host.facade()
        .update_current_replica_set_configuration(config(1, 1, &[]))
        .await
        .unwrap();
    execute(&sql, "CREATE TABLE t (v INTEGER)")
        .wait()
        .await
        .unwrap();
    sql.before_commit().arm();
    let mut write = execute(&sql, "INSERT INTO t VALUES (7)");
    sql.before_commit().wait_entered().await;
    assert_eq!(host.facade().current_progress().await.unwrap(), 2);
    assert_eq!(
        access.status().unwrap(),
        (AccessStatus::Granted, AccessStatus::Granted)
    );
    assert!(sql.query("SELECT * FROM t").is_err());
    std::fs::create_dir(root.path().join("state-v2.tmp")).unwrap();
    sql.before_commit().release();
    assert!(write.wait().await.is_err());
    assert_eq!(faults.lock().unwrap().len(), 1);
    assert_ne!(access.status().unwrap().0, AccessStatus::Granted);
    assert!(sql.begin_execute(vec![]).is_err());
    assert_eq!(host.facade().current_progress().await.unwrap(), 2);
    host.begin_close().unwrap();
    let HostOutcome::Closed(outcome) = host.resume().await.unwrap() else {
        panic!("outer Close")
    };
    assert!(matches!(
        outcome.application,
        crate::host::v1_lifecycle::ChildClose::Failed(_)
    ));
    host.join().await.unwrap();
    assert!(sql.close().await.is_err());
    std::fs::remove_dir(root.path().join("state-v2.tmp")).unwrap();
    drop(host);
    drop(sql);
    let reopened = store(root.path());
    reopened.promote(Arc::new(|_| unreachable!())).unwrap();
    assert_eq!(reopened.progress().unwrap(), 1);
    assert!(reopened.query("SELECT * FROM t").unwrap().is_empty());
    reopened.close().await.unwrap();
}

#[tokio::test]
async fn failed_secondary_persistence_withholds_native_ack_and_client_success() {
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let source_faults = Arc::new(Mutex::new(Vec::new()));
    let target_faults = Arc::new(Mutex::new(Vec::new()));
    let reported = source_faults.clone();
    let sql = Arc::new(
        Committer::open(
            a.path().into(),
            Arc::new(|_| Ok(())),
            Arc::new(move |reason| reported.lock().unwrap().push(reason)),
        )
        .unwrap(),
    );
    let reported = target_faults.clone();
    let target_sql = Arc::new(
        Committer::open(
            b.path().into(),
            Arc::new(|_| Ok(())),
            Arc::new(move |reason| reported.lock().unwrap().push(reason)),
        )
        .unwrap(),
    );
    let mut source = primary(sql.clone(), Bus::default(), MessageRegistry::default()).await;
    execute(&sql, "CREATE TABLE t (v INTEGER)")
        .wait()
        .await
        .unwrap();
    let mut target = copied(&mut source, target_sql.clone(), true).await;
    let mut replication = target.service.replication_stream().unwrap();
    let mut write = execute(&sql, "INSERT INTO t VALUES (7)");
    write.assigned().await.unwrap();
    source.bus.deliver(&source.registry);
    let operation = take(&mut replication).await;
    std::fs::create_dir(b.path().join("state-v2.tmp")).unwrap();
    let error = target_sql
        .apply_secondary(operation.lsn(), operation.data().clone())
        .unwrap_err();
    replication.report_fault(error.to_string()).unwrap();
    drop(operation);
    source.bus.deliver(&source.registry);
    assert!(matches!(
        target.adapter.current_progress().await,
        Err(AdapterError::Secondary(SecondaryError::Fault(_)))
    ));
    assert_eq!(source.adapter.current_progress().await.unwrap(), 1);
    assert_eq!(sql.progress().unwrap(), 1);
    assert!(write.wait().now_or_never().is_none());
    assert_eq!(target_faults.lock().unwrap().len(), 1);
    target.adapter.abort().unwrap();
    source.adapter.abort().unwrap();
    assert!(write.wait().await.is_err());
    assert!(sql.close().await.is_err());
    assert!(target_sql.close().await.is_err());
    source.adapter.drain_retired_builds().await.unwrap();
    target.adapter.drain_retired_builds().await.unwrap();
    assert_eq!(source_faults.lock().unwrap().len(), 1);
    std::fs::remove_dir(b.path().join("state-v2.tmp")).unwrap();
    let reopened = store(b.path());
    assert_eq!(reopened.progress().unwrap(), 1);
    reopened.close().await.unwrap();
}
