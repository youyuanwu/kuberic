use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationDescriptor, ConfigurationMember, Epoch, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
};

pub fn native_identity(id: i64, incarnation: &str) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(incarnation),
        agent_generation: AgentGeneration::new(format!("generation-{incarnation}")),
    }
}

pub fn native_configuration(
    members: &[ReplicaIdentity],
    primary: usize,
    epoch: i64,
) -> ConfigurationDescriptor {
    ConfigurationDescriptor::new(
        Epoch::new(0, epoch),
        members[primary].replica_id,
        members
            .iter()
            .enumerate()
            .map(|(index, identity)| ConfigurationMember {
                identity: identity.clone(),
                role: if index == primary {
                    ReplicaRole::Primary
                } else {
                    ReplicaRole::ActiveSecondary
                },
            })
            .collect(),
        members.len() as u32 / 2 + 1,
    )
}

pub fn find_pg_bin() -> PathBuf {
    for path in [
        "/usr/lib/postgresql/16/bin",
        "/usr/lib/postgresql/17/bin",
        "/usr/lib/postgresql/15/bin",
        "/usr/pgsql-16/bin",
        "/usr/local/pgsql/bin",
    ] {
        let path = PathBuf::from(path);
        if path.join("initdb").exists() {
            return path;
        }
    }
    if let Some(path) = std::process::Command::new("pg_config")
        .arg("--bindir")
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|path| !path.is_empty())
    {
        return PathBuf::from(path);
    }
    panic!("PostgreSQL binaries not found");
}

pub async fn allocate_port() -> u16 {
    static PORTS: OnceLock<Mutex<HashSet<u16>>> = OnceLock::new();
    loop {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral PostgreSQL port");
        let port = listener.local_addr().unwrap().port();
        let inserted = PORTS
            .get_or_init(|| Mutex::new(HashSet::new()))
            .lock()
            .unwrap()
            .insert(port);
        drop(listener);
        if inserted {
            return port;
        }
    }
}
