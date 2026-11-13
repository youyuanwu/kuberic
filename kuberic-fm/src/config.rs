use std::{collections::BTreeSet, fs, io::Read, net::SocketAddrV4, path::Path};

use anyhow::{ensure, Context, Result};
use fube_node_runner::{Fm, Identity, Placement};
use fube_ra::protocol::{
    ApplicationId, GenerationHeader, GenerationNumber, PackageId, PartitionId, Service,
    ServiceType, VersionInstance,
};
use fube_transport::serialization::serde::Guid;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub application: String,
    pub application_type: String,
    pub application_number: u32,
    pub package: String,
    pub service_type: String,
    pub name: String,
    pub package_version_instance: u64,
}

impl ServiceConfig {
    pub fn native(&self) -> Service {
        Service {
            name: self.name.clone(),
            instance: 1,
            service_type: ServiceType {
                package: PackageId {
                    application: ApplicationId {
                        type_name: self.application_type.clone(),
                        number: self.application_number,
                    },
                    name: self.package.clone(),
                },
                name: self.service_type.clone(),
            },
            application: self.application.clone(),
            version: VersionInstance {
                instance: self.package_version_instance,
                ..VersionInstance::default()
            },
            partitions: 1,
            target: 3,
            minimum: 2,
            stateful: true,
            persisted: false,
            ..Service::default()
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub schema_version: u32,
    pub deployment: String,
    pub fm: Fm,
    pub nodes: Vec<Placement>,
    pub partition: String,
    pub primary_node_id: String,
    pub service: ServiceConfig,
    #[serde(default = "retry_ms")]
    pub retry_interval_ms: u64,
    #[serde(default = "operation_ms")]
    pub operation_timeout_ms: u64,
    #[serde(default = "shutdown_ms")]
    pub shutdown_timeout_ms: u64,
}

fn retry_ms() -> u64 {
    15_000
}

fn operation_ms() -> u64 {
    120_000
}

fn shutdown_ms() -> u64 {
    30_000
}

fn address(value: SocketAddrV4) -> Result<()> {
    ensure!(
        value.port() != 0
            && !value.ip().is_unspecified()
            && !value.ip().is_multicast()
            && !value.ip().is_broadcast(),
        "control addresses must be concrete unicast IPv4 with nonzero ports"
    );
    Ok(())
}

impl Config {
    pub fn read(path: &Path) -> Result<Self> {
        ensure!(
            fs::symlink_metadata(path)?.is_file(),
            "configuration must be a regular file"
        );
        let mut bytes = Vec::new();
        fs::File::open(path)?.take(65_537).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 65_536, "configuration exceeds 64 KiB");
        let config: Self = serde_json::from_slice(&bytes).context("invalid FM configuration")?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "unsupported FM configuration schema"
        );
        ensure!(
            !uuid::Uuid::parse_str(&self.deployment)?.is_nil()
                && !uuid::Uuid::parse_str(&self.partition)?.is_nil(),
            "deployment and partition UUIDs must be nonnil"
        );
        ensure!(
            self.nodes.len() == 3,
            "only the original three-node 3/2 profile is supported"
        );
        address(self.fm.address)?;
        let fm = self.fm.identity.native().map_err(anyhow::Error::msg)?;
        ensure!(self.fm.generation > 0, "FM generation must be positive");
        let mut ids = BTreeSet::from([fm.id]);
        let mut ips = BTreeSet::new();
        let mut replicas = BTreeSet::new();
        for node in &self.nodes {
            address(node.address)?;
            let identity = node.identity.native().map_err(anyhow::Error::msg)?;
            ensure!(
                ids.insert(identity.id)
                    && ips.insert(*node.address.ip())
                    && replicas.insert(node.replica_id)
                    && node.address != self.fm.address
                    && node.replica_id >= 0
                    && node.replica_instance > 0,
                "node IDs, node IPv4 addresses and replica IDs must be distinct and valid"
            );
        }
        self.primary_index()?;
        let service = &self.service;
        ensure!(
            service.application.starts_with("fabric:/")
                && service.application.len() > "fabric:/".len()
                && service.application.len() <= 1024
                && service
                    .name
                    .starts_with(&format!("{}/", service.application))
                && service.name.len() > service.application.len() + 1
                && service.name.len() <= 1024
                && service.application_number > 0
                && service.package_version_instance > 0
                && [
                    &service.application_type,
                    &service.package,
                    &service.service_type
                ]
                .iter()
                .all(|value| !value.is_empty() && value.len() <= 128),
            "invalid application/service package description"
        );
        ensure!(
            (100..=600_000).contains(&self.operation_timeout_ms)
                && (1..=self.operation_timeout_ms).contains(&self.retry_interval_ms)
                && (100..=600_000).contains(&self.shutdown_timeout_ms),
            "invalid retry, operation or shutdown timeout"
        );
        Ok(())
    }

    pub fn primary_index(&self) -> Result<usize> {
        let primary = Identity {
            id: self.primary_node_id.clone(),
            instance: 1,
        }
        .native()
        .map_err(anyhow::Error::msg)?;
        self.nodes
            .iter()
            .position(|node| {
                node.identity
                    .native()
                    .is_ok_and(|identity| identity.id == primary.id)
            })
            .context("initial primary is not an original node")
    }

    pub fn generation(&self) -> Result<GenerationHeader> {
        Ok(GenerationHeader {
            number: GenerationNumber {
                owner: self.fm.identity.native().map_err(anyhow::Error::msg)?.id,
                generation: self.fm.generation,
            },
            for_fm_replica: false,
        })
    }

    pub fn partition_id(&self) -> Result<PartitionId> {
        Ok(PartitionId {
            guid: Guid(uuid::Uuid::parse_str(&self.partition)?.to_bytes_le()),
        })
    }
}
