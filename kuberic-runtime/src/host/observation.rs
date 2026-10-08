//! Private owner observations and narrow host-consumer projections.

use crate::authority::{AdmittedAuthority, RetiredAuthority};
use crate::effects::{BuildPostcondition, RoleTransition, RuntimeSnapshot};
use crate::host::state::AgentState;
use crate::protocol::types::{
    AccessStatus, ReplicaIdentity, ReplicaRole, SecondaryRemovalPreparation,
    SecondaryScaleDownCleanup,
};

/// Durable-agent facts used by reporting and recovery decisions.
///
/// This wrapper makes the durable aggregate's role explicit without exposing
/// process-local host or engine observations through the durable owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DurableAgentObservation(pub(crate) AgentState);

impl From<AgentState> for DurableAgentObservation {
    fn from(state: AgentState) -> Self {
        Self(state)
    }
}

impl std::ops::Deref for DurableAgentObservation {
    type Target = AgentState;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DurableAgentObservation {
    pub(crate) fn into_state(self) -> AgentState {
        self.0
    }
}

/// Host-proxy facts used by reporting.
///
/// These fields describe the process-local application/proxy projection. The
/// durable agent remains authoritative for desired authority, role, access,
/// and topology; the replication engine remains authoritative for progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HostProxyObservation {
    pub(crate) identity: ReplicaIdentity,
    pub(crate) open: bool,
    pub(crate) replication_address: Option<String>,
    pub(crate) role: ReplicaRole,
    pub(crate) role_transition: Option<RoleTransition>,
    pub(crate) read_status: AccessStatus,
    pub(crate) write_status: AccessStatus,
    pub(crate) authority: Option<AdmittedAuthority>,
    pub(crate) live_builds_only: bool,
}

/// Replication-engine facts used by reporting.
///
/// Engine-only fields are process-local and deliberately do not implement
/// durable serialization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReplicationEngineObservation {
    pub(crate) prepared_secondary_removal: Option<SecondaryRemovalPreparation>,
    pub(crate) retired_authority: Option<RetiredAuthority>,
    pub(crate) accepted_secondary_removal: Option<SecondaryScaleDownCleanup>,
    pub(crate) current_progress: i64,
    pub(crate) verified_replication_lsn: Option<i64>,
    pub(crate) committed_lsn: i64,
    pub(crate) current_configuration_quorum_progress: i64,
    pub(crate) catch_up_boundary: Option<i64>,
    pub(crate) catch_up_complete: bool,
    pub(crate) builds: Vec<BuildPostcondition>,
}

/// The explicit read-only report input assembled from the owning host and
/// replication-engine views.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReportObservation {
    pub(crate) host: HostProxyObservation,
    pub(crate) engine: ReplicationEngineObservation,
}

impl From<RuntimeSnapshot> for ReportObservation {
    fn from(snapshot: RuntimeSnapshot) -> Self {
        Self {
            host: HostProxyObservation {
                identity: snapshot.identity,
                open: snapshot.open,
                replication_address: snapshot.replication_address,
                role: snapshot.role,
                role_transition: snapshot.role_transition,
                read_status: snapshot.read_status,
                write_status: snapshot.write_status,
                authority: snapshot.authority,
                live_builds_only: snapshot.live_builds_only,
            },
            engine: ReplicationEngineObservation {
                prepared_secondary_removal: snapshot.prepared_secondary_removal,
                retired_authority: snapshot.retired_authority,
                accepted_secondary_removal: snapshot.accepted_secondary_removal,
                current_progress: snapshot.current_progress,
                verified_replication_lsn: snapshot.verified_replication_lsn,
                committed_lsn: snapshot.committed_lsn,
                current_configuration_quorum_progress: snapshot
                    .current_configuration_quorum_progress,
                catch_up_boundary: snapshot.catch_up_boundary,
                catch_up_complete: snapshot.catch_up_complete,
                builds: snapshot.builds,
            },
        }
    }
}

/// Build dispatch reads only live builds and current engine progress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BuildObservation {
    pub(crate) authority: Option<AdmittedAuthority>,
    pub(crate) builds: Vec<BuildPostcondition>,
    pub(crate) current_progress: i64,
}

/// Peer discovery reads only live authority and retirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PeerObservation {
    pub(crate) authority: Option<AdmittedAuthority>,
    pub(crate) retired_authority: Option<RetiredAuthority>,
}

/// Outbound validation reads only host liveness and live authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutboundObservation {
    pub(crate) open: bool,
    pub(crate) authority: Option<AdmittedAuthority>,
}

/// Restart recovery inspection needs only the actual write projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RecoveryObservation {
    pub(crate) read_status: AccessStatus,
    pub(crate) write_status: AccessStatus,
    pub(crate) authority: Option<AdmittedAuthority>,
}

impl From<RuntimeSnapshot> for RecoveryObservation {
    fn from(snapshot: RuntimeSnapshot) -> Self {
        Self {
            read_status: snapshot.read_status,
            write_status: snapshot.write_status,
            authority: snapshot.authority,
        }
    }
}

impl ReportObservation {
    pub(crate) fn build(&self) -> BuildObservation {
        BuildObservation {
            authority: self.host.authority.clone(),
            builds: self.engine.builds.clone(),
            current_progress: self.engine.current_progress,
        }
    }

    pub(crate) fn peer(&self) -> PeerObservation {
        PeerObservation {
            authority: self.host.authority.clone(),
            retired_authority: self.engine.retired_authority.clone(),
        }
    }

    pub(crate) fn outbound(&self) -> OutboundObservation {
        OutboundObservation {
            open: self.host.open,
            authority: self.host.authority.clone(),
        }
    }
}
