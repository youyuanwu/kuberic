use std::{fs, path::PathBuf, process::Command, time::Duration};

use kuberic_tests::process::{
    application, assert_application_stopped, crash_application, fm::Cluster, wait_replicated,
};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};

struct PausedApplication {
    pid: u64,
    executable: PathBuf,
}

impl PausedApplication {
    fn pause(cluster: &Cluster, node: usize) -> Self {
        let status = cluster.node_status(node);
        assert_eq!(status["deployment"], cluster.configs[node].deployment);
        let pid = status["application_pid"].as_u64().unwrap();
        let executable = fs::read_link(format!("/proc/{pid}/exe")).unwrap();
        assert!(executable.starts_with(cluster.configs[node].run_directory.join("hosting")));
        assert_eq!(executable.file_name().unwrap(), "hosting-local-app");
        assert!(Command::new("/bin/kill")
            .args(["-STOP", &pid.to_string()])
            .status()
            .unwrap()
            .success());
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
            let state = stat[stat.rfind(')').unwrap() + 1..]
                .split_whitespace()
                .next()
                .unwrap();
            if state == "T" {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "application did not pause: {stat}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        Self { pid, executable }
    }
}

impl Drop for PausedApplication {
    fn drop(&mut self) {
        let executable = match fs::read_link(format!("/proc/{}/exe", self.pid)) {
            Ok(executable) => executable,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => panic!(
                "observing paused original application {}: {error}",
                self.pid
            ),
        };
        assert_eq!(
            executable, self.executable,
            "paused application ownership changed"
        );
        assert!(Command::new("/bin/kill")
            .args(["-CONT", &self.pid.to_string()])
            .status()
            .unwrap()
            .success());
    }
}

fn barrier_work(status: &Value) -> PathBuf {
    PathBuf::from(status["runtime_root"].as_str().unwrap())
        .join("deployment/HostingLocalStatefulApp_App1/work")
}

fn hold(work: &std::path::Path, name: &str) {
    fs::write(
        work.join(format!("{name}.block")),
        b"hold actual native work",
    )
    .unwrap();
}

fn release(work: &std::path::Path, name: &str) {
    fs::write(
        work.join(format!("{name}.release")),
        b"release actual native work",
    )
    .unwrap();
}

async fn assert_no_write_success(reader: &mut BufReader<TcpStream>, response: &mut Vec<u8>) {
    match tokio::time::timeout(
        Duration::from_millis(500),
        reader.read_until(b'\n', response),
    )
    .await
    {
        Ok(Ok(bytes)) => {
            assert!(bytes == 0 || response.starts_with(b"ERR "), "{response:?}");
            eprintln!("no-quorum application response: {response:?}; bytes={bytes}");
        }
        Ok(Err(error)) => panic!("reading actual no-quorum application response: {error}"),
        Err(_) => eprintln!("no-quorum write did not complete within the observation window"),
    }
    assert!(!response.starts_with(b"OK"), "{response:?}");
}

#[tokio::test]
async fn lagging_paused_survivor_cannot_grant_failover_without_native_progress() {
    let mut cluster = Cluster::start_with_timeout(Duration::from_secs(5));
    let serving = cluster.until("serving").await;
    let endpoint = serving["service_endpoint"].as_str().unwrap();
    let paused = PausedApplication::pause(&cluster, 0);
    assert!(application(endpoint, "write").await.starts_with("OK"));
    let state: Value = serde_json::from_str(&application(endpoint, "state").await).unwrap();
    let last = state["provider"]["last"].as_i64().unwrap();
    let peers = state["peers"].as_array().unwrap();
    let lagging = peers.iter().find(|peer| peer["replica"] == 10).unwrap();
    let current = peers.iter().find(|peer| peer["replica"] == 30).unwrap();
    assert!(lagging["applied"].as_i64().unwrap() < last, "{state}");
    assert!(current["applied"].as_i64().unwrap() >= last, "{state}");
    assert!(application(endpoint, "read")
        .await
        .contains("value=acknowledged"));

    let lost = crash_application(&cluster.configs[1]);
    let refused = cluster.until_failed().await;
    assert!(refused["service_endpoint"].is_null(), "{refused}");
    assert!(
        refused["error"].as_str().unwrap().contains("timed out"),
        "{refused}"
    );
    assert_eq!(refused["primary_node_id"], "2");
    assert_application_stopped(lost);
    assert_eq!(cluster.node_status(0)["application_pid"], paused.pid);
    assert_ne!(cluster.node_status(2)["replica_role"], 4);
    eprintln!("lagging/unresponsive native progress refusal: {refused}");
    drop(paused);
    cluster.cleanup_failed().await;
}

#[tokio::test]
async fn secondary_quorum_loss_does_not_acknowledge_writes_or_promote_another_primary() {
    let mut cluster = Cluster::start_with_timeout(Duration::from_secs(5));
    let serving = cluster.until("serving").await;
    let endpoint = serving["service_endpoint"].as_str().unwrap();
    assert!(application(endpoint, "write").await.starts_with("OK"));
    wait_replicated(endpoint).await;
    let primary = cluster.node_status(1);
    let work = cluster.work(1);
    hold(&work, "local-current-configuration");
    let first = crash_application(&cluster.configs[0]);
    cluster
        .until_node(1, "held same-primary removal", |_| {
            work.join("local-current-configuration.held").exists()
        })
        .await;
    let second = crash_application(&cluster.configs[2]);
    assert_application_stopped(first);
    assert_application_stopped(second);
    let mut stream = TcpStream::connect(endpoint).await.unwrap();
    stream.write_all(b"write-more\n").await.unwrap();
    let mut reader = BufReader::new(stream);
    let mut response = Vec::new();
    assert_no_write_success(&mut reader, &mut response).await;
    let refused = cluster.until_failed().await;
    assert!(refused["service_endpoint"].is_null(), "{refused}");
    assert_eq!(refused["primary_node_id"], "2");
    assert_eq!(
        cluster.node_status(1)["application_pid"],
        primary["application_pid"]
    );
    assert_eq!(cluster.node_status(1)["replica_role"], 4);
    assert_no_write_success(&mut reader, &mut response).await;
    release(&work, "local-current-configuration");
    cluster.cleanup_failed().await;
    assert_no_write_success(&mut reader, &mut response).await;
}

#[tokio::test]
async fn crashing_a_copy_held_fresh_replica_never_restores_voting_membership() {
    interrupted_repair(false).await;
}

#[tokio::test]
async fn crashing_a_fresh_replica_during_final_native_admission_never_returns_serving() {
    interrupted_repair(true).await;
}

async fn interrupted_repair(final_admission: bool) {
    let mut cluster = Cluster::start_with_timeout(Duration::from_secs(10));
    let serving = cluster.until("serving").await;
    let endpoint = serving["service_endpoint"].as_str().unwrap();
    assert!(application(endpoint, "write").await.starts_with("OK"));
    wait_replicated(endpoint).await;
    let predecessor = cluster.node_status(0);
    cluster.hold_next_copy(0);
    crash_application(&cluster.configs[0]);
    let fresh = cluster
        .until_node(0, "actual fresh-host copy barrier", |status| {
            status["replica_id"].as_i64() > predecessor["replica_id"].as_i64()
                && barrier_work(status).join("local-copy.held").exists()
        })
        .await;
    let restoring = cluster.until("restoring").await;
    assert_eq!(restoring["primary_node_id"], "2");
    assert_eq!(restoring["service_endpoint"], endpoint);
    assert_eq!(fresh["replica_role"], 2);
    assert_eq!(fresh["local_configuration_complete"], false);
    let state: Value = serde_json::from_str(&application(endpoint, "state").await).unwrap();
    let idle = state["peers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|peer| peer["replica"] == fresh["replica_id"])
        .unwrap();
    assert_eq!(idle["configured"], false, "{state}");
    assert_eq!(idle["building"], true, "{state}");
    let primary_work = cluster.work(1);
    if final_admission {
        hold(&primary_work, "local-current-configuration");
        release(&barrier_work(&fresh), "local-copy");
        cluster
            .until_node(1, "held final native membership callback", |_| {
                primary_work
                    .join("local-current-configuration.held")
                    .exists()
            })
            .await;
        assert_eq!(cluster.status().unwrap()["phase"], "restoring");
    }
    let lost = crash_application(&cluster.configs[0]);
    assert_eq!(lost, fresh["application_pid"].as_u64().unwrap());
    let refused = cluster.until_failed().await;
    assert!(refused["service_endpoint"].is_null(), "{refused}");
    assert_eq!(refused["primary_node_id"], "2");
    assert_application_stopped(lost);
    if final_admission {
        release(&primary_work, "local-current-configuration");
    }
    eprintln!("interrupted actual copy/admission refusal: {refused}");
    cluster.cleanup_failed().await;
}
