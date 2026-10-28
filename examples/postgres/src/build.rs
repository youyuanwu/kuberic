use crate::native::PgReplicationEvidence;
use std::path::Path;

use kuberic_runtime::protocol::types::{BuildAuthority, ProcessSessionId, ResourceUid};
use serde::{Deserialize, Serialize};

use crate::instance::PgError;
use crate::monitor::parse_pg_lsn;

pub const BUILD_PROTOCOL_VERSION: u32 = 2;
pub const MAX_ENVELOPE_BYTES: usize = 65536;
pub const MAX_BUILDS: usize = 16;
const MAX_HISTORY: usize = 64;

pub(crate) fn native_build_selected(
    request: &PgBuildRequest,
    configuration: &kuberic_runtime::replicator::ReplicaSetConfiguration,
) -> bool {
    configuration.configuration == request.authority.current_configuration
        && configuration.replicas.iter().any(|replica| {
            replica.identity == request.authority.source
                && replica.process_session_id == request.source_session
                && replica.role == kuberic_runtime::protocol::types::ReplicaRole::Primary
        })
        && configuration.replicas.iter().any(|replica| {
            replica.identity == request.authority.target
                && replica.process_session_id == request.target_session
                && replica.role == kuberic_runtime::protocol::types::ReplicaRole::IdleSecondary
                && replica.current_progress == request.authority.replication_boundary_lsn
        })
}

#[cfg(feature = "testing")]
pub struct BuildGate {
    pub stage: PgBuildStage,
    pub entered: tokio::sync::Notify,
    pub release: tokio::sync::Notify,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineFork {
    pub timeline: u32,
    pub end_lsn: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgLineage {
    pub system_identifier: String,
    pub timeline: u32,
    pub history: Vec<TimelineFork>,
    pub history_text: String,
}

impl PgLineage {
    pub(crate) fn prefix_is_contained_by(
        &self,
        source: &Self,
        required_lsn: i64,
        source_flush_lsn: i64,
    ) -> bool {
        if self.validate().is_err()
            || source.validate().is_err()
            || self.system_identifier != source.system_identifier
            || required_lsn < 0
            || source_flush_lsn < required_lsn
        {
            return false;
        }
        let target_segments = self
            .history
            .iter()
            .map(|fork| (fork.timeline, fork.end_lsn))
            .chain(std::iter::once((self.timeline, i64::MAX)));
        let source_segments = source
            .history
            .iter()
            .map(|fork| (fork.timeline, fork.end_lsn))
            .chain(std::iter::once((source.timeline, i64::MAX)));
        // Equal system IDs/LSNs cannot prove inclusion across a timeline fork.
        for ((target_timeline, target_end), (source_timeline, source_end)) in
            target_segments.zip(source_segments)
        {
            if target_timeline != source_timeline {
                return false;
            }
            if required_lsn <= target_end.min(source_end) {
                return true;
            }
            if target_end != source_end {
                return false;
            }
        }
        false
    }

    pub fn validate(&self) -> Result<(), String> {
        if self
            .system_identifier
            .parse::<u64>()
            .ok()
            .is_none_or(|id| id == 0)
            || self.system_identifier.len() > 20
            || self.timeline == 0
            || self.history.len() > MAX_HISTORY
            || (self.timeline == 1) != self.history.is_empty()
            || self.history_text.len() > 16384
        {
            return Err("invalid PostgreSQL lineage".into());
        }
        if Self::parse_history(&self.history_text).map_err(|e| e.to_string())? != self.history {
            return Err("timeline history text differs from parsed lineage".into());
        }
        let mut timeline = 0;
        let mut boundary = 0;
        for fork in &self.history {
            if fork.timeline <= timeline
                || fork.timeline >= self.timeline
                || fork.end_lsn <= 0
                || fork.end_lsn < boundary
            {
                return Err("unordered or duplicate PostgreSQL history".into());
            }
            timeline = fork.timeline;
            boundary = fork.end_lsn;
        }
        Ok(())
    }

    pub fn can_rewind_from(&self, source: &Self) -> bool {
        self.system_identifier == source.system_identifier
            && (self == source
                || source
                    .history
                    .iter()
                    .any(|fork| fork.timeline == self.timeline)
                    && source.history.starts_with(&self.history))
    }

    pub async fn read(
        data: &Path,
        system_identifier: String,
        timeline: u32,
    ) -> Result<Self, PgError> {
        let mut history_text = String::new();
        if timeline > 1 {
            let path = data.join("pg_wal").join(format!("{timeline:08X}.history"));
            let metadata = tokio::fs::metadata(&path)
                .await
                .map_err(|e| PgError::Configuration(format!("timeline history: {e}")))?;
            if metadata.len() > 16384 {
                return Err(PgError::Configuration("oversized timeline history".into()));
            }
            history_text = tokio::fs::read_to_string(path)
                .await
                .map_err(|e| PgError::Configuration(format!("timeline history: {e}")))?;
        }
        let history = Self::parse_history(&history_text)?;
        let lineage = Self {
            system_identifier,
            timeline,
            history,
            history_text,
        };
        lineage.validate().map_err(PgError::Configuration)?;
        Ok(lineage)
    }

    fn parse_history(text: &str) -> Result<Vec<TimelineFork>, PgError> {
        let mut history = Vec::new();
        for line in text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
        {
            let mut fields = line.split_whitespace();
            let timeline = fields
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| PgError::Configuration("invalid history timeline".into()))?;
            let end_lsn = parse_pg_lsn(
                fields
                    .next()
                    .ok_or_else(|| PgError::Configuration("missing history boundary".into()))?,
            )?;
            history.push(TimelineFork { timeline, end_lsn });
        }
        Ok(history)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgBuildRequest {
    pub version: u32,
    pub resource_uid: ResourceUid,
    pub authority: BuildAuthority,
    pub source_session: ProcessSessionId,
    pub target_session: ProcessSessionId,
    pub source_endpoint: String,
    pub source_host: String,
    pub source_port: u16,
    pub lineage: PgLineage,
}

impl PgBuildRequest {
    pub fn validate(&self) -> Result<(), String> {
        self.authority.validate().map_err(|e| e.to_string())?;
        self.lineage.validate()?;
        if self.version != BUILD_PROTOCOL_VERSION
            || self.source_port == 0
            || self.authority.source == self.authority.target
            || self.authority.build_id.is_empty()
            || self.authority.current_configuration.members.len() > 32
        {
            return Err("invalid native build envelope".into());
        }
        for value in [
            self.resource_uid.as_str(),
            self.authority.build_id.as_str(),
            self.source_session.as_str(),
            self.target_session.as_str(),
            self.source_endpoint.as_str(),
            self.source_host.as_str(),
            self.authority.source.instance_id.as_str(),
            self.authority.source.agent_generation.as_str(),
            self.authority.target.instance_id.as_str(),
            self.authority.target.agent_generation.as_str(),
        ] {
            if value.is_empty() || value.len() > 512 || value.chars().any(char::is_control) {
                return Err("missing or oversized native build identity/endpoint".into());
            }
        }
        if !self.source_endpoint.starts_with("http://")
            || self.source_host.parse::<std::net::IpAddr>().is_err()
        {
            return Err("native build requires an explicit source IP and HTTP endpoint".into());
        }
        Ok(())
    }

    pub fn same_work(&self, other: &Self) -> bool {
        self.authority == other.authority
            && self.resource_uid == other.resource_uid
            && self.lineage == other.lineage
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PgBuildMethod {
    Fresh,
    Rewind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PgBuildStage {
    Intent,
    Copying,
    Installed,
    Recovering,
    Complete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PgBuildProgress {
    pub request: PgBuildRequest,
    pub stage: PgBuildStage,
    pub method: PgBuildMethod,
    pub sequence: u64,
    pub evidence: Option<PgReplicationEvidence>,
}

impl PgBuildProgress {
    pub fn advance(&mut self, stage: PgBuildStage) -> Result<(), String> {
        self.sequence = self
            .sequence
            .checked_add(1)
            .ok_or("native build sequence overflow")?;
        self.stage = stage;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        self.request.validate()?;
        if self.sequence == 0 {
            return Err("missing native build sequence".into());
        }
        if let Some(evidence) = &self.evidence {
            evidence.validate()?;
            if evidence.engine != "postgres-physical"
                || evidence.system_identifier != self.request.lineage.system_identifier
                || evidence.timeline_id != self.request.lineage.timeline
                || !evidence.in_recovery
            {
                return Err("native build lineage differs from frozen source".into());
            }
        }
        if self.stage == PgBuildStage::Complete && !self.recovered() {
            return Err("native build is not durably replayed at its frozen boundary".into());
        }
        Ok(())
    }

    pub fn recovered(&self) -> bool {
        let boundary = self.request.authority.replication_boundary_lsn;
        self.evidence.as_ref().is_some_and(|e| {
            e.engine == "postgres-physical"
                && e.system_identifier == self.request.lineage.system_identifier
                && e.timeline_id == self.request.lineage.timeline
                && e.in_recovery
                && e.flush_lsn >= boundary
                && e.replay_lsn.is_some_and(|replay| replay >= boundary)
        })
    }
}

pub fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, String> {
    let bytes = serde_json::to_vec(value).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_ENVELOPE_BYTES {
        return Err("oversized native envelope".into());
    }
    Ok(bytes)
}

pub fn decode<T: serde::de::DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T, String> {
    if bytes.is_empty() || bytes.len() > MAX_ENVELOPE_BYTES {
        return Err("invalid native envelope length".into());
    }
    let value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if encode(&value)? != bytes {
        return Err("native envelope must use canonical encoding".into());
    }
    Ok(value)
}

#[cfg(test)]
mod lineage_tests {
    use super::*;

    fn lineage(timeline: u32, forks: &[(u32, i64)]) -> PgLineage {
        PgLineage {
            system_identifier: "12345".into(),
            timeline,
            history: forks
                .iter()
                .map(|(timeline, end_lsn)| TimelineFork {
                    timeline: *timeline,
                    end_lsn: *end_lsn,
                })
                .collect(),
            history_text: forks
                .iter()
                .map(|(timeline, end_lsn)| format!("{timeline}\t0/{end_lsn:X}\ttest fork\n"))
                .collect(),
        }
    }

    #[test]
    fn m3_pg_native_prefix_proof_requires_fork_inclusion_not_just_system_identifier() {
        let retained = lineage(1, &[]);
        let source = lineage(2, &[(1, 200)]);
        assert!(retained.prefix_is_contained_by(&source, 200, 300));
        assert!(!retained.prefix_is_contained_by(&source, 201, 300));
        assert!(!retained.prefix_is_contained_by(&source, 200, 199));

        let diverged = lineage(3, &[(1, 100)]);
        assert!(diverged.prefix_is_contained_by(&source, 100, 300));
        assert!(!diverged.prefix_is_contained_by(&source, 101, 300));
        assert!(!source.prefix_is_contained_by(&diverged, 201, 300));

        let child = lineage(4, &[(1, 200), (2, 400)]);
        assert!(source.prefix_is_contained_by(&child, 400, 500));
        assert!(!source.prefix_is_contained_by(&child, 401, 500));
        assert!(child.prefix_is_contained_by(&child, 500, 500));
        let mut foreign = child.clone();
        foreign.system_identifier = "98765".into();
        assert!(!child.prefix_is_contained_by(&foreign, 300, 500));
        assert!(!child.prefix_is_contained_by(&child, -1, 500));
        let mut inconsistent = child.clone();
        inconsistent.history_text.clear();
        assert!(!inconsistent.prefix_is_contained_by(&child, 300, 500));
    }
}
