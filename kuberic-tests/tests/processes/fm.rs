use std::{
    fs,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
};

use kuberic_tests::process::{
    application, assert_application_stopped, crash_application, finish, fm::Cluster, terminate,
    wait_replicated,
};
use serde_json::Value;

#[tokio::test]
async fn real_fm_and_fube_nodes_create_serve_delete_and_join_owned_processes() {
    lifecycle(Fault::None).await;
}

#[tokio::test]
async fn real_primary_application_crash_preserves_data_continues_writes_and_cleans_original_nodes()
{
    lifecycle(Fault::Primary).await;
}

#[tokio::test]
async fn real_secondary_application_crashes_rebuild_fresh_replicas_without_changing_primary() {
    lifecycle(Fault::Secondary).await;
}

#[tokio::test]
async fn real_secondary_restoration_accepts_successful_forced_predecessor_cleanup() {
    lifecycle(Fault::ForcedSecondary).await;
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fault {
    None,
    Primary,
    Secondary,
    ForcedSecondary,
}

struct GroupHelper(Child);

impl GroupHelper {
    fn join(pid: u64) -> Self {
        Self(
            Command::new("/bin/sleep")
                .arg("30")
                .process_group(i32::try_from(pid).unwrap())
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    }
}

impl Drop for GroupHelper {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

fn node_status(config: &fube_node_runner::Config) -> Value {
    serde_json::from_slice(&fs::read(config.run_directory.join("status.json")).unwrap()).unwrap()
}

async fn lifecycle(fault: Fault) {
    let mut cluster = Cluster::start();
    let status = cluster.until("serving").await;
    let endpoint = status["service_endpoint"].as_str().unwrap();
    assert!(application(endpoint, "write").await.starts_with("OK"));
    assert!(application(endpoint, "read")
        .await
        .contains("value=acknowledged"));

    if fault == Fault::Primary {
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
        let written = application(next, "write-more").await;
        assert!(
            written.starts_with("OK"),
            "{written}; {}",
            cluster.diagnostics()
        );
        assert!(application(next, "read").await.contains("value=continued"));
        assert_application_stopped(lost);
        assert!(tokio::net::TcpStream::connect(endpoint).await.is_err());
        let restored: Value = serde_json::from_slice(
            &fs::read(cluster.configs[1].run_directory.join("status.json")).unwrap(),
        )
        .unwrap();
        assert!(restored["replica_id"].as_i64().unwrap() > cluster.configs[1].node.replica_id);
        assert!(
            restored["replica_instance"].as_i64().unwrap()
                > cluster.configs[1].node.replica_instance
        );
        assert_eq!(restored["retired_applications"][0]["pid"], lost);
        assert_eq!(restored["retired_applications"][0]["reaped"], true);
        assert_eq!(restored["retired_applications"][0]["group_drained"], true);
        assert_eq!(restored["retired_applications"][0]["root_removed"], true);
        wait_replicated(next).await;
        let second_lost = crash_application(&cluster.configs[0]);
        let again = cluster
            .until_status("second restored failover", |status| {
                status["phase"] == "serving" && status["primary_node_id"] == "2"
            })
            .await;
        let next = again["service_endpoint"].as_str().unwrap();
        assert!(application(next, "read").await.contains("value=continued"));
        let written = application(next, "write-final").await;
        assert!(
            written.starts_with("OK"),
            "{written}; {}",
            cluster.diagnostics()
        );
        assert!(application(next, "read").await.contains("value=final"));
        assert_application_stopped(second_lost);
    }

    if matches!(fault, Fault::Secondary | Fault::ForcedSecondary) {
        for (cycle, victim, command, value) in [
            (0, 0, "write-more", "continued"),
            (1, 0, "write-final", "final"),
            (2, 2, "read", "final"),
        ] {
            wait_replicated(endpoint).await;
            let before: Vec<_> = cluster.configs.iter().map(node_status).collect();
            let forced = cycle == 0 && fault == Fault::ForcedSecondary;
            let mut helper = forced
                .then(|| GroupHelper::join(before[victim]["application_pid"].as_u64().unwrap()));
            let lost = crash_application(&cluster.configs[victim]);
            let destination = cluster.configs[victim].clone();
            let restored = cluster
                .until_status("secondary repair", |status| {
                    let fresh = node_status(&destination);
                    status["phase"] == "serving"
                        && fresh["replica_id"].as_i64() > before[victim]["replica_id"].as_i64()
                        && fresh["local_configuration_complete"] == true
                })
                .await;
            assert_eq!(restored["primary_node_id"], "2");
            assert_eq!(restored["service_endpoint"], endpoint);
            assert!(application(endpoint, command).await.starts_with("OK"));
            assert!(application(endpoint, "read")
                .await
                .contains(&format!("value={value}")));
            let fresh = node_status(&destination);
            assert!(
                fresh["replica_instance"].as_i64() > before[victim]["replica_instance"].as_i64()
            );
            assert_eq!(fresh["replica_role"], 3);
            assert_ne!(fresh["runtime_root"], before[victim]["runtime_root"]);
            assert!(!Path::new(before[victim]["runtime_root"].as_str().unwrap()).exists());
            let retired = fresh["retired_applications"]
                .as_array()
                .unwrap()
                .iter()
                .find(|retired| retired["pid"] == lost)
                .unwrap();
            assert_eq!(retired["reaped"], true);
            assert_eq!(retired["group_drained"], true);
            assert_eq!(retired["root_removed"], true);
            assert_eq!(retired["forced"], forced);
            if let Some(helper) = &mut helper {
                assert!(helper.0.try_wait().unwrap().is_some());
            }
            for (index, config) in cluster.configs.iter().enumerate() {
                if index != victim {
                    assert_eq!(
                        node_status(config)["application_pid"],
                        before[index]["application_pid"]
                    );
                }
            }
            assert_application_stopped(lost);
            assert!(!Path::new("/proc").join(lost.to_string()).exists());
        }
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
