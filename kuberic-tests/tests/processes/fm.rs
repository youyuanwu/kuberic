use std::{fs, path::Path};

use kuberic_tests::process::{
    application, assert_application_stopped, crash_application, finish, fm::Cluster, terminate,
    wait_replicated,
};
use serde_json::Value;

#[tokio::test]
async fn real_fm_and_fube_nodes_create_serve_delete_and_join_owned_processes() {
    lifecycle(false).await;
}

#[tokio::test]
async fn real_primary_application_crash_preserves_data_continues_writes_and_cleans_original_nodes()
{
    lifecycle(true).await;
}

async fn lifecycle(failover: bool) {
    let mut cluster = Cluster::start();
    let status = cluster.until("serving").await;
    let endpoint = status["service_endpoint"].as_str().unwrap();
    assert!(application(endpoint, "write").await.starts_with("OK"));
    assert!(application(endpoint, "read")
        .await
        .contains("value=acknowledged"));

    if failover {
        wait_replicated(endpoint).await;
        let lost = crash_application(&cluster.configs[1]);
        let promoted = cluster
            .until_status("failover", |status| {
                status["phase"] == "serving" && status["primary_node_id"] != "2"
            })
            .await;
        let next = promoted["service_endpoint"].as_str().unwrap();
        assert_ne!(next, endpoint);
        assert!(application(next, "read")
            .await
            .contains("value=acknowledged"));
        assert!(application(next, "write-more").await.starts_with("OK"));
        assert!(application(next, "read").await.contains("value=continued"));
        assert_application_stopped(lost);
        assert!(tokio::net::TcpStream::connect(endpoint).await.is_err());
    }

    terminate(&cluster.fm);
    let status = cluster.until("awaiting_node_shutdown").await;
    assert!(status["service_endpoint"].is_null());
    for node in &cluster.nodes {
        terminate(node);
    }
    for node in &mut cluster.nodes {
        finish(node).await;
    }
    finish(&mut cluster.fm).await;
    assert_eq!(cluster.status().unwrap()["phase"], "stopped");
    for config in &cluster.configs {
        let status: Value =
            serde_json::from_slice(&fs::read(config.run_directory.join("status.json")).unwrap())
                .unwrap();
        assert_eq!(status["stage"], "stopped");
        assert_eq!(status["cleanup"]["clean"], true, "{status}");
        let pid = status["application_pid"].as_u64().unwrap();
        assert!(!Path::new("/proc").join(pid.to_string()).exists());
    }
}
