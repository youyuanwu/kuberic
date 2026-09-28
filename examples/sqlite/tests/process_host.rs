use std::sync::Arc;
use std::time::Duration;

use kuberic_agent::process::{ApplicationStorageState, ReplicaHost, ReplicaProcessConfig};
use kuberic_agent::transport::ReplicaEndpointResolver;
use kuberic_protocol::types::{PodUid, PvcUid, ReplicaId, ReplicaIdentity, ResourceUid};
use sqlite_replicated::proto::sqlite_store_server::SqliteStore as _;
use sqlite_replicated::server::SqliteServer;
use sqlite_replicated::service::SqliteService;
use sqlite_replicated::testing::{SqlitePod, scratch};
use sqlite_replicated::{SqlitePersistence, proto};

struct SingletonResolver;
impl ReplicaEndpointResolver for SingletonResolver {
    fn control_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
    fn replication_endpoint(&self, _: &ReplicaIdentity) -> String {
        "http://127.0.0.1:1".into()
    }
}

#[tokio::test]
async fn replica_host_reopens_sqlite_with_new_process_sessions_without_external_services() {
    let root = scratch();
    let pod = SqlitePod::singleton(root.path().join("replica")).await;
    pod.execute("CREATE TABLE data(id INTEGER)").await.unwrap();
    pod.execute("INSERT INTO data VALUES(42)").await.unwrap();
    let path = pod.root.clone();
    drop(pod);
    let mut last_session = None;
    for _ in 0..2 {
        let persistence = Arc::new(SqlitePersistence::open(path.join("application")).unwrap());
        let application =
            Arc::new(SqliteService::new(persistence, "http://127.0.0.1:0".into()).unwrap());
        let mut replica = ReplicaHost::new(
            ReplicaProcessConfig {
                resource_uid: ResourceUid::new("sqlite-test"),
                replica_id: ReplicaId::new(1),
                pod_uid: PodUid::new("sqlite-1"),
                pvc_uid: PvcUid::new("sqlite-pvc-1"),
                data_root: path.clone(),
                control_address: "127.0.0.1:0".parse().unwrap(),
                replication_address: "127.0.0.1:0".parse().unwrap(),
                bearer_token: "in-process-test-token".into(),
                rpc_deadline: Duration::from_secs(2),
                transport_window_capacity: 16,
            },
            application.clone(),
            ApplicationStorageState::Established,
            Arc::new(SingletonResolver),
        )
        .start()
        .await
        .unwrap();
        let diagnostics = replica.handle().diagnostics().await.unwrap();
        assert_eq!(diagnostics.committed_lsn, 2);
        assert_ne!(last_session.as_ref(), Some(&diagnostics.process_session));
        last_session = Some(diagnostics.process_session);
        let server = SqliteServer::new(application);
        let response = server
            .query(tonic::Request::new(proto::QueryRequest {
                sql: "SELECT id FROM data".into(),
                params: Vec::new(),
            }))
            .await
            .unwrap()
            .into_inner();
        assert_eq!(
            response.rows[0].values[0].kind,
            Some(proto::value::Kind::IntegerValue(42))
        );
        replica.shutdown();
        tokio::time::timeout(Duration::from_secs(5), replica.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(
            server
                .query(tonic::Request::new(proto::QueryRequest {
                    sql: "SELECT id FROM data".into(),
                    params: Vec::new(),
                }))
                .await
                .is_err()
        );
    }
}
