//! Strict validation and conversion between protobuf and canonical protocol types.

use crate::protocol::command::{
    InitializeAgentStore, ProtocolCommand, PublicV1HistoryCommand, PublicV1OperationCommand,
};
use crate::protocol::public_operations::{
    CanonicalOperationInputDigest, CanonicalOperationInputDigestV1,
    CanonicalReplicaRebuildRequired, CapturedPublicOperationIdentity, DataLossAttempt,
    DataLossReplay, ProcessAttemptContext, ProviderProgressReset, ProviderReplicaIdentity,
    PublicBuildAttemptReference, PublicCatchUpMode, PublicDataLossRecoveryStage,
    PublicDataLossRecoveryStatus, PublicOperationReplayDisposition, PublicProgress,
    PublicProgressRole, PublicV1AgentReport, PublicV1CallKind, PublicV1ExtensionSlot,
    PublicV1Extensions, PublicV1Identity, PublicV1OperationCompletion, PublicV1OperationResult,
    ReplicaRebuildReason, ServiceLocation,
};
use crate::protocol::types::{
    AccessStatus, AgentGeneration, BuildAuthority, BuildAuthorityKind, ConfigurationDescriptor,
    ConfigurationId, ConfigurationMember, EffectivePolicy, Epoch, HistoryContext, HistoryId,
    InitializationId, OpenMode, OperationId, PodUid, ProcessSessionId, ProvisioningIntent,
    ProvisioningKind, ProvisioningPurpose, PublicProtocolGeneration, PvcUid, ReplicaId,
    ReplicaIdentity, ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity,
    ReplicaStorageIncarnationContext, ReplicatorCreationId, ResourceUid, StorageGeneration,
    SwitchoverHandoff, SwitchoverRequestId,
};
use crate::protocol::validation::validate_configuration;
use thiserror::Error;

use crate::control::proto;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WireError {
    #[error("unsupported protocol version {observed}; expected {expected}")]
    UnsupportedProtocolVersion { expected: u32, observed: u32 },
    #[error("missing required field {0}")]
    MissingField(&'static str),
    #[error("invalid enum value {value} for {field}")]
    InvalidEnum { field: &'static str, value: i32 },
    #[error("invalid wire authority: {0}")]
    InvalidAuthority(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecuteEnvelope {
    pub resource_uid: ResourceUid,
    pub target: ReplicaIdentity,
    pub expected_process_session_id: ProcessSessionId,
    pub command: ProtocolCommand,
}

pub fn public_v1_history_command_to_proto(
    command: &PublicV1HistoryCommand,
) -> Result<proto::PublicV1HistoryCommand, WireError> {
    command
        .validate()
        .map_err(|message| WireError::InvalidAuthority(message.into()))?;
    Ok(proto::PublicV1HistoryCommand {
        canonical_json: serde_json::to_vec(command)
            .map_err(|error| WireError::InvalidAuthority(error.to_string()))?,
    })
}

pub fn public_v1_operation_command_to_proto(
    command: &PublicV1OperationCommand,
) -> Result<proto::PublicV1OperationCommand, WireError> {
    command
        .validate()
        .map_err(|message| WireError::InvalidAuthority(message.into()))?;
    Ok(proto::PublicV1OperationCommand {
        canonical_json: serde_json::to_vec(command)
            .map_err(|error| WireError::InvalidAuthority(error.to_string()))?,
    })
}

fn public_v1_history_command_from_proto(
    command: proto::PublicV1HistoryCommand,
) -> Result<PublicV1HistoryCommand, WireError> {
    if command.canonical_json.is_empty() {
        return Err(WireError::MissingField("public_v1_history.canonical_json"));
    }
    let command: PublicV1HistoryCommand = serde_json::from_slice(&command.canonical_json)
        .map_err(|error| WireError::InvalidAuthority(error.to_string()))?;
    command
        .validate()
        .map_err(|message| WireError::InvalidAuthority(message.into()))?;
    Ok(command)
}

fn public_v1_operation_command_from_proto(
    command: proto::PublicV1OperationCommand,
) -> Result<PublicV1OperationCommand, WireError> {
    if command.canonical_json.is_empty() {
        return Err(WireError::MissingField(
            "public_v1_operation.canonical_json",
        ));
    }
    let command: PublicV1OperationCommand = serde_json::from_slice(&command.canonical_json)
        .map_err(|error| WireError::InvalidAuthority(error.to_string()))?;
    command
        .validate()
        .map_err(|message| WireError::InvalidAuthority(message.into()))?;
    Ok(command)
}

/// Requires an exact protocol-version match; negotiation is intentionally unsupported.
pub fn ensure_supported_version(observed: u32) -> Result<(), WireError> {
    if observed == crate::protocol::PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(WireError::UnsupportedProtocolVersion {
            expected: crate::protocol::PROTOCOL_VERSION,
            observed,
        })
    }
}

pub fn normalize_uninitialized_agent_status_report(
    report: proto::AgentStatusReport,
) -> Result<crate::protocol::observation::UninitializedAgentObservation, WireError> {
    if report.protocol_version != crate::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION {
        return Err(WireError::UnsupportedProtocolVersion {
            expected: crate::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION,
            observed: report.protocol_version,
        });
    }
    if report.public_v1.is_some() {
        return Err(WireError::InvalidAuthority(
            "fresh agent status cannot carry established Public V1 evidence".into(),
        ));
    }
    let storage_state = proto::AgentStorageState::try_from(report.storage_state).map_err(|_| {
        WireError::InvalidEnum {
            field: "agent_status.storage_state",
            value: report.storage_state,
        }
    })?;
    if storage_state != proto::AgentStorageState::Uninitialized
        || !report.healthy
        || !report.storage_error.is_empty()
        || report.resource_uid.is_empty()
        || report.process_session_id.is_empty()
        || report.replica_id <= 0
        || report.pod_uid.is_empty()
        || report.pvc_uid.is_empty()
    {
        return Err(WireError::InvalidAuthority(
            "fresh agent status is incomplete or unsafe".into(),
        ));
    }
    Ok(
        crate::protocol::observation::UninitializedAgentObservation {
            protocol_version: report.protocol_version,
            resource_uid: ResourceUid::new(report.resource_uid),
            replica_id: ReplicaId::new(report.replica_id),
            pod_uid: PodUid::new(report.pod_uid),
            pvc_uid: PvcUid::new(report.pvc_uid),
            process_session_id: ProcessSessionId::new(report.process_session_id),
            report_sequence: 1,
        },
    )
}

/// Normalizes the production Public V1 report branch.
pub fn normalize_public_v1_agent_status_report(
    report: proto::AgentStatusReport,
) -> Result<PublicV1AgentReport, WireError> {
    if report.protocol_version != crate::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION {
        return Err(WireError::UnsupportedProtocolVersion {
            expected: crate::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION,
            observed: report.protocol_version,
        });
    }
    if outer_initialization_fields_present(&report) {
        return Err(WireError::InvalidAuthority(
            "public-v1 status cannot carry legacy or native evidence".into(),
        ));
    }
    let public = report
        .public_v1
        .ok_or(WireError::MissingField("agent_status.public_v1"))?;
    if public.protocol_version != crate::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION
        || public.generation == 0
    {
        return Err(WireError::InvalidAuthority(
            "public-v1 report identity is invalid".into(),
        ));
    }
    let identity = public
        .identity
        .ok_or(WireError::MissingField("public_v1.identity"))?
        .try_into()?;
    let role = proto::ReplicaRole::try_from(public.role)
        .map_err(|_| WireError::InvalidEnum {
            field: "public_v1.role",
            value: public.role,
        })
        .and_then(role_from_proto)?;
    let read_status = proto::AccessStatus::try_from(public.read_status)
        .map_err(|_| WireError::InvalidEnum {
            field: "public_v1.read_status",
            value: public.read_status,
        })
        .and_then(access_status_from_proto)?;
    let write_status = proto::AccessStatus::try_from(public.write_status)
        .map_err(|_| WireError::InvalidEnum {
            field: "public_v1.write_status",
            value: public.write_status,
        })
        .and_then(access_status_from_proto)?;
    let progress_role =
        proto::PublicV1ProgressRole::try_from(public.progress_role).map_err(|_| {
            WireError::InvalidEnum {
                field: "public_v1.progress_role",
                value: public.progress_role,
            }
        })?;
    let progress_role = match progress_role {
        proto::PublicV1ProgressRole::Primary => PublicProgressRole::Primary,
        proto::PublicV1ProgressRole::Secondary => PublicProgressRole::Secondary,
        proto::PublicV1ProgressRole::Recovery => PublicProgressRole::Recovery,
        proto::PublicV1ProgressRole::Unknown => {
            return Err(WireError::InvalidEnum {
                field: "public_v1.progress_role",
                value: public.progress_role,
            });
        }
    };
    let open_mode = match proto::PublicV1OpenMode::try_from(public.open_mode) {
        Ok(proto::PublicV1OpenMode::New) => OpenMode::New,
        Ok(proto::PublicV1OpenMode::Existing) => OpenMode::Existing,
        Ok(proto::PublicV1OpenMode::Unknown) | Err(_) => {
            return Err(WireError::InvalidEnum {
                field: "public_v1.open_mode",
                value: public.open_mode,
            });
        }
    };
    let completed_operations = public
        .completed_operations
        .into_iter()
        .map(public_v1_completion_from_proto)
        .collect::<Result<Vec<_>, _>>()?;
    let extensions = PublicV1Extensions {
        catch_up_specific_quorum: public.catch_up_specific_quorum,
        history_recovery: public
            .history_recovery_extension_version
            .map(extension_slot)
            .transpose()?,
        public_build_evidence: public
            .public_build_evidence_extension_version
            .map(extension_slot)
            .transpose()?,
    };
    let history = public_v1_history_from_proto(
        public
            .history
            .ok_or(WireError::MissingField("public_v1.history"))?,
    )?;
    let storage_incarnation = public_v1_storage_from_proto(
        public
            .storage_incarnation
            .ok_or(WireError::MissingField("public_v1.storage_incarnation"))?,
    )?;
    let data_loss_recovery = public
        .data_loss_recovery
        .map(public_v1_recovery_from_proto)
        .transpose()?;
    let service_location = public
        .service_location
        .map(|location| {
            Ok(ServiceLocation {
                identity: PublicV1Identity {
                    protocol_version: location.protocol_version,
                    generation: location.generation,
                },
                resource_uid: ResourceUid::new(location.resource_uid),
                operation_id: OperationId::new(location.operation_id),
                replica: location
                    .replica
                    .ok_or(WireError::MissingField(
                        "public_v1.service_location.replica",
                    ))?
                    .try_into()?,
                process_session_id: ProcessSessionId::new(location.process_session_id),
                epoch: location
                    .epoch
                    .ok_or(WireError::MissingField("public_v1.service_location.epoch"))?
                    .into(),
                revision: location.revision,
                address: location.address,
            })
        })
        .transpose()?;
    let report = PublicV1AgentReport {
        identity: PublicV1Identity {
            protocol_version: public.protocol_version,
            generation: public.generation,
        },
        resource_uid: ResourceUid::new(public.resource_uid),
        replica: identity,
        process_session_id: ProcessSessionId::new(public.process_session_id),
        replicator_creation_id: ReplicatorCreationId::new(public.replicator_creation_id),
        report_sequence: public.report_sequence,
        role,
        read_status,
        write_status,
        history,
        storage_incarnation,
        progress: PublicProgress {
            role: progress_role,
            current_progress: progress_query_from_proto(public.current_progress)?,
            catch_up_capability: progress_query_from_proto(public.catch_up_capability)?,
        },
        configuration: public
            .configuration_json
            .map(|json| {
                serde_json::from_str(&json).map_err(|error| {
                    WireError::InvalidAuthority(format!("invalid installed configuration: {error}"))
                })
            })
            .transpose()?,
        active_operations: public
            .active_operations_json
            .into_iter()
            .map(|json| {
                serde_json::from_str(&json).map_err(|error| {
                    WireError::InvalidAuthority(format!(
                        "invalid active control observation: {error}"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        retry_pending_operations: public
            .retry_pending_operations_json
            .into_iter()
            .map(|json| {
                serde_json::from_str(&json).map_err(|error| {
                    WireError::InvalidAuthority(format!(
                        "invalid retry-pending control observation: {error}"
                    ))
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        replication_address: public.replication_address,
        open_mode,
        service_location,
        data_loss_recovery,
        completed_operations,
        extensions,
    };
    report
        .validate()
        .map_err(|message| WireError::InvalidAuthority(message.into()))?;
    Ok(report)
}

/// Encodes only the structurally public protocol-11 branch. All legacy/native
/// fields in the outer envelope remain at their protobuf defaults.
pub fn public_v1_agent_status_to_proto(report: &PublicV1AgentReport) -> proto::AgentStatusReport {
    debug_assert_eq!(
        crate::protocol::public_operations::PUBLIC_V1_PROTOCOL_REGISTRATIONS,
        &["PUBLIC_V1_PROTOCOL_VERSION"]
    );
    proto::AgentStatusReport {
        protocol_version: crate::protocol::public_operations::PUBLIC_V1_PROTOCOL_VERSION,
        public_v1: Some(proto::PublicV1AgentStatus {
            protocol_version: report.identity.protocol_version,
            generation: report.identity.generation,
            resource_uid: report.resource_uid.as_str().to_string(),
            identity: Some(report.replica.clone().into()),
            process_session_id: report.process_session_id.as_str().to_string(),
            replicator_creation_id: report.replicator_creation_id.as_str().to_string(),
            report_sequence: report.report_sequence,
            open_mode: match report.open_mode {
                OpenMode::New => proto::PublicV1OpenMode::New as i32,
                OpenMode::Existing => proto::PublicV1OpenMode::Existing as i32,
            },
            replication_address: report.replication_address.clone(),
            configuration_json: report.configuration.as_ref().map(|configuration| {
                serde_json::to_string(configuration)
                    .expect("public configuration serialization is infallible")
            }),
            active_operations_json: report
                .active_operations
                .iter()
                .map(|intent| {
                    serde_json::to_string(intent)
                        .expect("control observation serialization is infallible")
                })
                .collect(),
            retry_pending_operations_json: report
                .retry_pending_operations
                .iter()
                .map(|pending| {
                    serde_json::to_string(pending)
                        .expect("retry-pending observation serialization is infallible")
                })
                .collect(),
            role: role_to_proto(report.role) as i32,
            read_status: access_status_to_proto(report.read_status) as i32,
            write_status: access_status_to_proto(report.write_status) as i32,
            progress_role: match report.progress.role {
                PublicProgressRole::Primary => proto::PublicV1ProgressRole::Primary as i32,
                PublicProgressRole::Secondary => proto::PublicV1ProgressRole::Secondary as i32,
                PublicProgressRole::Recovery => proto::PublicV1ProgressRole::Recovery as i32,
            },
            current_progress: Some(progress_query_to_proto(&report.progress.current_progress)),
            catch_up_capability: Some(progress_query_to_proto(
                &report.progress.catch_up_capability,
            )),
            completed_operations: report
                .completed_operations
                .iter()
                .map(public_v1_completion_to_proto)
                .collect(),
            catch_up_specific_quorum: report.extensions.catch_up_specific_quorum,
            history_recovery_extension_version: report
                .extensions
                .history_recovery
                .as_ref()
                .map(|slot| u32::from(slot.version)),
            public_build_evidence_extension_version: report
                .extensions
                .public_build_evidence
                .as_ref()
                .map(|slot| u32::from(slot.version)),
            history: Some(public_v1_history_to_proto(&report.history)),
            storage_incarnation: Some(public_v1_storage_to_proto(&report.storage_incarnation)),
            data_loss_recovery: report
                .data_loss_recovery
                .as_ref()
                .map(public_v1_recovery_to_proto),
            service_location: report.service_location.as_ref().map(|location| {
                proto::PublicV1ServiceLocation {
                    protocol_version: location.identity.protocol_version,
                    generation: location.identity.generation,
                    resource_uid: location.resource_uid.as_str().to_string(),
                    operation_id: location.operation_id.as_str().to_string(),
                    replica: Some(location.replica.clone().into()),
                    process_session_id: location.process_session_id.as_str().to_string(),
                    epoch: Some(location.epoch.into()),
                    revision: location.revision,
                    address: location.address.clone(),
                }
            }),
        }),
        ..Default::default()
    }
}

fn extension_slot(version: u32) -> Result<PublicV1ExtensionSlot, WireError> {
    Ok(PublicV1ExtensionSlot {
        version: u16::try_from(version).map_err(|_| {
            WireError::InvalidAuthority("public-v1 extension version exceeds u16".into())
        })?,
    })
}

pub(crate) fn public_v1_history_from_proto(
    value: proto::PublicV1HistoryContext,
) -> Result<HistoryContext, WireError> {
    let history = HistoryContext {
        resource_uid: ResourceUid::new(value.resource_uid),
        protocol_generation: PublicProtocolGeneration::new(value.protocol_generation),
        history_id: HistoryId::new(value.history_id),
        data_loss_number: value.data_loss_number,
    };
    history
        .validate()
        .map_err(|message| WireError::InvalidAuthority(message.into()))?;
    Ok(history)
}

pub(crate) fn public_v1_history_to_proto(value: &HistoryContext) -> proto::PublicV1HistoryContext {
    proto::PublicV1HistoryContext {
        resource_uid: value.resource_uid.as_str().to_string(),
        protocol_generation: value.protocol_generation.value(),
        history_id: value.history_id.as_str().to_string(),
        data_loss_number: value.data_loss_number,
    }
}

pub(crate) fn public_v1_storage_from_proto(
    value: proto::PublicV1StorageIncarnation,
) -> Result<ReplicaStorageIncarnationContext, WireError> {
    let kind =
        proto::PublicV1StorageKind::try_from(value.kind).map_err(|_| WireError::InvalidEnum {
            field: "public_v1.storage.kind",
            value: value.kind,
        })?;
    let storage = match kind {
        proto::PublicV1StorageKind::Persistent => {
            if !value.volatile_identity.is_empty() {
                return Err(WireError::InvalidAuthority(
                    "persistent public-v1 storage carries volatile identity".into(),
                ));
            }
            ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new(value.pod_uid),
                pvc_uid: PvcUid::new(value.pvc_uid),
            }
        }
        proto::PublicV1StorageKind::Volatile => {
            if !value.pod_uid.is_empty() || !value.pvc_uid.is_empty() {
                return Err(WireError::InvalidAuthority(
                    "volatile public-v1 storage carries persistent identity".into(),
                ));
            }
            ReplicaStorageIdentity::Volatile {
                identity: value.volatile_identity,
            }
        }
        proto::PublicV1StorageKind::Unknown => {
            return Err(WireError::InvalidEnum {
                field: "public_v1.storage.kind",
                value: value.kind,
            });
        }
    };
    let incarnation = ReplicaStorageIncarnationContext {
        replica: value
            .replica
            .ok_or(WireError::MissingField("public_v1.storage.replica"))?
            .try_into()?,
        storage,
        storage_generation: StorageGeneration::new(value.storage_generation),
    };
    incarnation
        .validate()
        .map_err(|message| WireError::InvalidAuthority(message.into()))?;
    Ok(incarnation)
}

pub(crate) fn public_v1_storage_to_proto(
    value: &ReplicaStorageIncarnationContext,
) -> proto::PublicV1StorageIncarnation {
    let (kind, pod_uid, pvc_uid, volatile_identity) = match &value.storage {
        ReplicaStorageIdentity::Persistent { pod_uid, pvc_uid } => (
            proto::PublicV1StorageKind::Persistent,
            pod_uid.as_str().to_string(),
            pvc_uid.as_str().to_string(),
            String::new(),
        ),
        ReplicaStorageIdentity::Volatile { identity } => (
            proto::PublicV1StorageKind::Volatile,
            String::new(),
            String::new(),
            identity.clone(),
        ),
    };
    proto::PublicV1StorageIncarnation {
        replica: Some(value.replica.clone().into()),
        kind: kind as i32,
        pod_uid,
        pvc_uid,
        volatile_identity,
        storage_generation: value.storage_generation.as_str().to_string(),
    }
}

fn public_v1_attempt_from_proto(
    value: proto::PublicV1DataLossAttempt,
) -> Result<DataLossAttempt, WireError> {
    let replay = replay_from_proto(value.data_loss_replay)?;
    let PublicOperationReplayDisposition::DataLoss(data_loss_replay) = replay else {
        return Err(WireError::InvalidAuthority(
            "data-loss attempt carries a non-data-loss replay disposition".into(),
        ));
    };
    Ok(DataLossAttempt {
        authorization_id: OperationId::new(value.authorization_id),
        attempt_id: OperationId::new(value.attempt_id),
        process: ProcessAttemptContext {
            process_session_id: ProcessSessionId::new(value.process_session_id),
            replicator_creation_id: ReplicatorCreationId::new(value.replicator_creation_id),
            catch_up_specific_quorum: value.catch_up_specific_quorum,
            data_loss_replay,
            epoch: value
                .epoch
                .ok_or(WireError::MissingField("public_v1.recovery.attempt.epoch"))?
                .into(),
            attempt_revision: value.attempt_revision,
        },
    })
}

fn public_v1_attempt_to_proto(value: &DataLossAttempt) -> proto::PublicV1DataLossAttempt {
    proto::PublicV1DataLossAttempt {
        authorization_id: value.authorization_id.as_str().to_string(),
        attempt_id: value.attempt_id.as_str().to_string(),
        process_session_id: value.process.process_session_id.as_str().to_string(),
        replicator_creation_id: value.process.replicator_creation_id.as_str().to_string(),
        catch_up_specific_quorum: value.process.catch_up_specific_quorum,
        data_loss_replay: replay_to_proto(PublicOperationReplayDisposition::DataLoss(
            value.process.data_loss_replay,
        )) as i32,
        epoch: Some(value.process.epoch.into()),
        attempt_revision: value.process.attempt_revision,
    }
}

fn public_v1_recovery_from_proto(
    value: proto::PublicV1DataLossRecoveryStatus,
) -> Result<PublicDataLossRecoveryStatus, WireError> {
    let reset_kind = proto::PublicV1ProviderProgressResetKind::try_from(value.reset_scope_kind)
        .map_err(|_| WireError::InvalidEnum {
            field: "public_v1.recovery.reset_scope_kind",
            value: value.reset_scope_kind,
        })?;
    let reset_scope = match (reset_kind, value.retired_storage) {
        (proto::PublicV1ProviderProgressResetKind::Preserve, None) => {
            ProviderProgressReset::Preserve
        }
        (proto::PublicV1ProviderProgressResetKind::ResetToEmpty, None) => {
            ProviderProgressReset::ResetToEmpty
        }
        (proto::PublicV1ProviderProgressResetKind::ReplaceIncarnation, Some(retired)) => {
            ProviderProgressReset::ReplaceIncarnation {
                retired: public_v1_storage_from_proto(retired)?,
            }
        }
        (proto::PublicV1ProviderProgressResetKind::Unknown, _) => {
            return Err(WireError::InvalidEnum {
                field: "public_v1.recovery.reset_scope_kind",
                value: value.reset_scope_kind,
            });
        }
        _ => {
            return Err(WireError::InvalidAuthority(
                "public-v1 recovery reset scope is noncanonical".into(),
            ));
        }
    };
    let replay = replay_from_proto(value.authorization_replay)?;
    let PublicOperationReplayDisposition::DataLoss(replay_disposition) = replay else {
        return Err(WireError::InvalidAuthority(
            "public-v1 recovery authorization has a non-data-loss replay disposition".into(),
        ));
    };
    let stage = proto::PublicV1DataLossRecoveryStage::try_from(value.stage).map_err(|_| {
        WireError::InvalidEnum {
            field: "public_v1.recovery.stage",
            value: value.stage,
        }
    })?;
    let stage = match stage {
        proto::PublicV1DataLossRecoveryStage::Authorized => PublicDataLossRecoveryStage::Authorized,
        proto::PublicV1DataLossRecoveryStage::Invalidated => {
            PublicDataLossRecoveryStage::Invalidated
        }
        proto::PublicV1DataLossRecoveryStage::CallbackProcessing => {
            PublicDataLossRecoveryStage::CallbackProcessing
        }
        proto::PublicV1DataLossRecoveryStage::HistoryPending => {
            PublicDataLossRecoveryStage::HistoryPending
        }
        proto::PublicV1DataLossRecoveryStage::ConfigurationPending => {
            PublicDataLossRecoveryStage::ConfigurationPending
        }
        proto::PublicV1DataLossRecoveryStage::Admitted => PublicDataLossRecoveryStage::Admitted,
        proto::PublicV1DataLossRecoveryStage::Failed => PublicDataLossRecoveryStage::Failed,
        proto::PublicV1DataLossRecoveryStage::ReplacementRequired => {
            PublicDataLossRecoveryStage::ReplacementRequired
        }
        proto::PublicV1DataLossRecoveryStage::Retired => PublicDataLossRecoveryStage::Retired,
        proto::PublicV1DataLossRecoveryStage::Unknown => {
            return Err(WireError::InvalidEnum {
                field: "public_v1.recovery.stage",
                value: value.stage,
            });
        }
    };
    let observed_progress = value
        .observed_progress
        .map(|progress| {
            let role = proto::PublicV1ProgressRole::try_from(progress.role).map_err(|_| {
                WireError::InvalidEnum {
                    field: "public_v1.recovery.progress.role",
                    value: progress.role,
                }
            })?;
            let role = match role {
                proto::PublicV1ProgressRole::Primary => PublicProgressRole::Primary,
                proto::PublicV1ProgressRole::Secondary => PublicProgressRole::Secondary,
                proto::PublicV1ProgressRole::Recovery => PublicProgressRole::Recovery,
                proto::PublicV1ProgressRole::Unknown => {
                    return Err(WireError::InvalidEnum {
                        field: "public_v1.recovery.progress.role",
                        value: progress.role,
                    });
                }
            };
            Ok(PublicProgress {
                role,
                current_progress: progress_query_from_proto(progress.current_progress)?,
                catch_up_capability: progress_query_from_proto(progress.catch_up_capability)?,
            })
        })
        .transpose()?;
    Ok(PublicDataLossRecoveryStatus {
        authorization_id: OperationId::new(value.authorization_id),
        attempt: public_v1_attempt_from_proto(
            value
                .attempt
                .ok_or(WireError::MissingField("public_v1.recovery.attempt"))?,
        )?,
        predecessor_history: public_v1_history_from_proto(value.predecessor_history.ok_or(
            WireError::MissingField("public_v1.recovery.predecessor_history"),
        )?)?,
        proposed_history: public_v1_history_from_proto(value.proposed_history.ok_or(
            WireError::MissingField("public_v1.recovery.proposed_history"),
        )?)?,
        provisional_primary: public_v1_storage_from_proto(value.provisional_primary.ok_or(
            WireError::MissingField("public_v1.recovery.provisional_primary"),
        )?)?,
        reset_scope,
        replay_disposition,
        stage,
        observed_progress,
    })
}

fn public_v1_recovery_to_proto(
    value: &PublicDataLossRecoveryStatus,
) -> proto::PublicV1DataLossRecoveryStatus {
    let (reset_scope_kind, retired_storage) = match &value.reset_scope {
        ProviderProgressReset::Preserve => {
            (proto::PublicV1ProviderProgressResetKind::Preserve, None)
        }
        ProviderProgressReset::ResetToEmpty => {
            (proto::PublicV1ProviderProgressResetKind::ResetToEmpty, None)
        }
        ProviderProgressReset::ReplaceIncarnation { retired } => (
            proto::PublicV1ProviderProgressResetKind::ReplaceIncarnation,
            Some(public_v1_storage_to_proto(retired)),
        ),
    };
    proto::PublicV1DataLossRecoveryStatus {
        authorization_id: value.authorization_id.as_str().to_string(),
        attempt: Some(public_v1_attempt_to_proto(&value.attempt)),
        predecessor_history: Some(public_v1_history_to_proto(&value.predecessor_history)),
        proposed_history: Some(public_v1_history_to_proto(&value.proposed_history)),
        provisional_primary: Some(public_v1_storage_to_proto(&value.provisional_primary)),
        reset_scope_kind: reset_scope_kind as i32,
        retired_storage,
        authorization_replay: replay_to_proto(PublicOperationReplayDisposition::DataLoss(
            value.replay_disposition,
        )) as i32,
        stage: match value.stage {
            PublicDataLossRecoveryStage::Authorized => {
                proto::PublicV1DataLossRecoveryStage::Authorized
            }
            PublicDataLossRecoveryStage::Invalidated => {
                proto::PublicV1DataLossRecoveryStage::Invalidated
            }
            PublicDataLossRecoveryStage::CallbackProcessing => {
                proto::PublicV1DataLossRecoveryStage::CallbackProcessing
            }
            PublicDataLossRecoveryStage::HistoryPending => {
                proto::PublicV1DataLossRecoveryStage::HistoryPending
            }
            PublicDataLossRecoveryStage::ConfigurationPending => {
                proto::PublicV1DataLossRecoveryStage::ConfigurationPending
            }
            PublicDataLossRecoveryStage::Admitted => proto::PublicV1DataLossRecoveryStage::Admitted,
            PublicDataLossRecoveryStage::Failed => proto::PublicV1DataLossRecoveryStage::Failed,
            PublicDataLossRecoveryStage::ReplacementRequired => {
                proto::PublicV1DataLossRecoveryStage::ReplacementRequired
            }
            PublicDataLossRecoveryStage::Retired => proto::PublicV1DataLossRecoveryStage::Retired,
        } as i32,
        observed_progress: value.observed_progress.as_ref().map(|progress| {
            proto::PublicV1RecoveryProgress {
                role: match progress.role {
                    PublicProgressRole::Primary => proto::PublicV1ProgressRole::Primary,
                    PublicProgressRole::Secondary => proto::PublicV1ProgressRole::Secondary,
                    PublicProgressRole::Recovery => proto::PublicV1ProgressRole::Recovery,
                } as i32,
                current_progress: Some(progress_query_to_proto(&progress.current_progress)),
                catch_up_capability: Some(progress_query_to_proto(&progress.catch_up_capability)),
            }
        }),
    }
}

fn progress_query_from_proto(
    query: Option<proto::PublicV1ProgressQuery>,
) -> Result<crate::protocol::public_operations::ProgressQueryResult, WireError> {
    use crate::protocol::public_operations::ProgressQueryResult as Query;
    use proto::public_v1_progress_query::Result as Wire;
    let outcome = match query.and_then(|query| query.result) {
        Some(Wire::Value(value)) => Query::Value(value),
        Some(Wire::NotReady(true)) => Query::NotReady,
        Some(Wire::Failure(reason)) if !reason.is_empty() => Query::Failed(reason),
        Some(Wire::NotQueried(reason)) if !reason.is_empty() => Query::NotQueried(reason),
        _ => {
            return Err(WireError::InvalidAuthority(
                "missing or invalid native query outcome".into(),
            ));
        }
    };
    outcome
        .validate()
        .map_err(|error| WireError::InvalidAuthority(error.into()))?;
    Ok(outcome)
}

fn progress_query_to_proto(
    query: &crate::protocol::public_operations::ProgressQueryResult,
) -> proto::PublicV1ProgressQuery {
    use crate::protocol::public_operations::ProgressQueryResult as Query;
    use proto::public_v1_progress_query::Result as Wire;
    proto::PublicV1ProgressQuery {
        result: Some(match query {
            Query::Value(value) => Wire::Value(*value),
            Query::NotReady => Wire::NotReady(true),
            Query::NotQueried(reason) => Wire::NotQueried(reason.clone()),
            Query::Failed(reason) => Wire::Failure(reason.clone()),
        }),
    }
}

fn outer_initialization_fields_present(report: &proto::AgentStatusReport) -> bool {
    !report.resource_uid.is_empty()
        || !report.process_session_id.is_empty()
        || report.storage_state != proto::AgentStorageState::Unknown as i32
        || !report.pod_uid.is_empty()
        || !report.pvc_uid.is_empty()
        || !report.storage_error.is_empty()
        || report.healthy
        || report.replica_id != 0
}

fn replay_from_proto(value: i32) -> Result<PublicOperationReplayDisposition, WireError> {
    let value =
        proto::PublicV1ReplayDisposition::try_from(value).map_err(|_| WireError::InvalidEnum {
            field: "public_v1.completion.replay_disposition",
            value,
        })?;
    Ok(match value {
        proto::PublicV1ReplayDisposition::Convergent => {
            PublicOperationReplayDisposition::Convergent
        }
        proto::PublicV1ReplayDisposition::RepeatableRead => {
            PublicOperationReplayDisposition::RepeatableRead
        }
        proto::PublicV1ReplayDisposition::ReevaluateCatchUp => {
            PublicOperationReplayDisposition::ReevaluateCatchUp
        }
        proto::PublicV1ReplayDisposition::RetainBuildSuccessRetireAmbiguity => {
            PublicOperationReplayDisposition::RetainBuildSuccessRetireAmbiguity
        }
        proto::PublicV1ReplayDisposition::DataLossConvergent => {
            PublicOperationReplayDisposition::DataLoss(DataLossReplay::Convergent)
        }
        proto::PublicV1ReplayDisposition::DataLossReplaceOnAmbiguity => {
            PublicOperationReplayDisposition::DataLoss(DataLossReplay::ReplaceOnAmbiguity)
        }
        proto::PublicV1ReplayDisposition::ExactRemoval => {
            PublicOperationReplayDisposition::ExactRemoval
        }
        proto::PublicV1ReplayDisposition::Containment => {
            PublicOperationReplayDisposition::Containment
        }
        proto::PublicV1ReplayDisposition::Unknown => {
            return Err(WireError::InvalidEnum {
                field: "public_v1.completion.replay_disposition",
                value: value as i32,
            });
        }
    })
}

fn replay_to_proto(
    disposition: PublicOperationReplayDisposition,
) -> proto::PublicV1ReplayDisposition {
    match disposition {
        PublicOperationReplayDisposition::Convergent => {
            proto::PublicV1ReplayDisposition::Convergent
        }
        PublicOperationReplayDisposition::RepeatableRead => {
            proto::PublicV1ReplayDisposition::RepeatableRead
        }
        PublicOperationReplayDisposition::ReevaluateCatchUp => {
            proto::PublicV1ReplayDisposition::ReevaluateCatchUp
        }
        PublicOperationReplayDisposition::RetainBuildSuccessRetireAmbiguity => {
            proto::PublicV1ReplayDisposition::RetainBuildSuccessRetireAmbiguity
        }
        PublicOperationReplayDisposition::DataLoss(DataLossReplay::Convergent) => {
            proto::PublicV1ReplayDisposition::DataLossConvergent
        }
        PublicOperationReplayDisposition::DataLoss(DataLossReplay::ReplaceOnAmbiguity) => {
            proto::PublicV1ReplayDisposition::DataLossReplaceOnAmbiguity
        }
        PublicOperationReplayDisposition::ExactRemoval => {
            proto::PublicV1ReplayDisposition::ExactRemoval
        }
        PublicOperationReplayDisposition::Containment => {
            proto::PublicV1ReplayDisposition::Containment
        }
    }
}

fn rebuild_reason_from_proto(value: i32) -> Result<ReplicaRebuildReason, WireError> {
    let value =
        proto::PublicV1RebuildReason::try_from(value).map_err(|_| WireError::InvalidEnum {
            field: "public_v1.rebuild.reason",
            value,
        })?;
    Ok(match value {
        proto::PublicV1RebuildReason::IncompatibleRetainedHistory => {
            ReplicaRebuildReason::IncompatibleRetainedHistory
        }
        proto::PublicV1RebuildReason::ProviderMismatch => ReplicaRebuildReason::ProviderMismatch,
        proto::PublicV1RebuildReason::RemoteCopyConflict => {
            ReplicaRebuildReason::RemoteCopyConflict
        }
        proto::PublicV1RebuildReason::Unknown => {
            return Err(WireError::InvalidEnum {
                field: "public_v1.rebuild.reason",
                value: value as i32,
            });
        }
    })
}

fn rebuild_reason_to_proto(reason: ReplicaRebuildReason) -> proto::PublicV1RebuildReason {
    match reason {
        ReplicaRebuildReason::IncompatibleRetainedHistory => {
            proto::PublicV1RebuildReason::IncompatibleRetainedHistory
        }
        ReplicaRebuildReason::ProviderMismatch => proto::PublicV1RebuildReason::ProviderMismatch,
        ReplicaRebuildReason::RemoteCopyConflict => {
            proto::PublicV1RebuildReason::RemoteCopyConflict
        }
    }
}

fn captured_operation_from_proto(
    value: proto::PublicV1CapturedOperationIdentity,
) -> Result<CapturedPublicOperationIdentity, WireError> {
    let epoch = value
        .epoch
        .ok_or(WireError::MissingField("public_v1.rebuild.operation.epoch"))?
        .into();
    let identity = CapturedPublicOperationIdentity {
        resource_uid: ResourceUid::new(value.resource_uid),
        identity: PublicV1Identity {
            protocol_version: value.protocol_version,
            generation: value.generation,
        },
        epoch,
        operation_id: OperationId::new(value.operation_id),
        revision: value.revision,
        detecting_session: ProcessSessionId::new(value.detecting_session),
    };
    identity
        .validate()
        .map_err(|message| WireError::InvalidAuthority(message.into()))?;
    Ok(identity)
}

fn captured_operation_to_proto(
    value: &CapturedPublicOperationIdentity,
) -> proto::PublicV1CapturedOperationIdentity {
    proto::PublicV1CapturedOperationIdentity {
        resource_uid: value.resource_uid.as_str().to_string(),
        protocol_version: value.identity.protocol_version,
        generation: value.identity.generation,
        epoch: Some(value.epoch.into()),
        operation_id: value.operation_id.as_str().to_string(),
        revision: value.revision,
        detecting_session: value.detecting_session.as_str().to_string(),
    }
}

fn build_attempt_from_proto(
    value: proto::PublicV1BuildAttemptReference,
) -> Result<PublicBuildAttemptReference, WireError> {
    let authorization = PublicBuildAttemptReference {
        resource_uid: ResourceUid::new(value.resource_uid),
        epoch: value
            .epoch
            .ok_or(WireError::MissingField(
                "public_v1.rebuild.authorization.epoch",
            ))?
            .into(),
        build_id: OperationId::new(value.build_id),
        source: value
            .source
            .ok_or(WireError::MissingField(
                "public_v1.rebuild.authorization.source",
            ))?
            .try_into()?,
        source_storage: public_v1_storage_from_proto(value.source_storage.ok_or(
            WireError::MissingField("public_v1.rebuild.authorization.source_storage"),
        )?)?,
        source_session: ProcessSessionId::new(value.source_session),
        target: value
            .target
            .ok_or(WireError::MissingField(
                "public_v1.rebuild.authorization.target",
            ))?
            .try_into()?,
        target_storage: public_v1_storage_from_proto(value.target_storage.ok_or(
            WireError::MissingField("public_v1.rebuild.authorization.target_storage"),
        )?)?,
        target_session: ProcessSessionId::new(value.target_session),
    };
    authorization
        .validate()
        .map_err(|message| WireError::InvalidAuthority(message.into()))?;
    Ok(authorization)
}

fn build_attempt_to_proto(
    value: &PublicBuildAttemptReference,
) -> proto::PublicV1BuildAttemptReference {
    proto::PublicV1BuildAttemptReference {
        resource_uid: value.resource_uid.as_str().to_string(),
        epoch: Some(value.epoch.into()),
        build_id: value.build_id.as_str().to_string(),
        source: Some(value.source.clone().into()),
        source_storage: Some(public_v1_storage_to_proto(&value.source_storage)),
        source_session: value.source_session.as_str().to_string(),
        target: Some(value.target.clone().into()),
        target_storage: Some(public_v1_storage_to_proto(&value.target_storage)),
        target_session: value.target_session.as_str().to_string(),
    }
}

fn rebuild_from_proto(
    value: proto::PublicV1ReplicaRebuildRequired,
) -> Result<CanonicalReplicaRebuildRequired, WireError> {
    let kind =
        proto::PublicV1RebuildKind::try_from(value.kind).map_err(|_| WireError::InvalidEnum {
            field: "public_v1.rebuild.kind",
            value: value.kind,
        })?;
    let reason = rebuild_reason_from_proto(value.reason)?;
    let operation = captured_operation_from_proto(
        value
            .operation
            .ok_or(WireError::MissingField("public_v1.rebuild.operation"))?,
    )?;
    let rebuild = match kind {
        proto::PublicV1RebuildKind::LocalProvider => {
            if value.build.is_some() {
                return Err(WireError::InvalidAuthority(
                    "local rebuild failure contains remote authorization".into(),
                ));
            }
            CanonicalReplicaRebuildRequired::LocalProvider {
                reason,
                provider: ProviderReplicaIdentity {
                    replica: value
                        .provider_replica
                        .ok_or(WireError::MissingField(
                            "public_v1.rebuild.provider_replica",
                        ))?
                        .try_into()?,
                },
                operation,
                local_replica: value
                    .local_replica
                    .ok_or(WireError::MissingField("public_v1.rebuild.local_replica"))?
                    .try_into()?,
                local_storage: PvcUid::new(value.local_storage),
            }
        }
        proto::PublicV1RebuildKind::RemoteBuildTarget => {
            if value.provider_replica.is_some()
                || value.local_replica.is_some()
                || !value.local_storage.is_empty()
            {
                return Err(WireError::InvalidAuthority(
                    "remote rebuild failure contains local-provider identity".into(),
                ));
            }
            CanonicalReplicaRebuildRequired::RemoteBuildTarget {
                reason,
                operation,
                build: Box::new(build_attempt_from_proto(
                    value
                        .build
                        .ok_or(WireError::MissingField("public_v1.rebuild.build"))?,
                )?),
            }
        }
        proto::PublicV1RebuildKind::Unknown => {
            return Err(WireError::InvalidEnum {
                field: "public_v1.rebuild.kind",
                value: value.kind,
            });
        }
    };
    rebuild
        .validate()
        .map_err(|message| WireError::InvalidAuthority(message.into()))?;
    Ok(rebuild)
}

fn rebuild_to_proto(
    value: &CanonicalReplicaRebuildRequired,
) -> proto::PublicV1ReplicaRebuildRequired {
    match value {
        CanonicalReplicaRebuildRequired::LocalProvider {
            reason,
            provider,
            operation,
            local_replica,
            local_storage,
        } => proto::PublicV1ReplicaRebuildRequired {
            kind: proto::PublicV1RebuildKind::LocalProvider as i32,
            reason: rebuild_reason_to_proto(*reason) as i32,
            operation: Some(captured_operation_to_proto(operation)),
            provider_replica: Some(provider.replica.clone().into()),
            local_replica: Some(local_replica.clone().into()),
            local_storage: local_storage.as_str().to_string(),
            build: None,
        },
        CanonicalReplicaRebuildRequired::RemoteBuildTarget {
            reason,
            operation,
            build,
        } => proto::PublicV1ReplicaRebuildRequired {
            kind: proto::PublicV1RebuildKind::RemoteBuildTarget as i32,
            reason: rebuild_reason_to_proto(*reason) as i32,
            operation: Some(captured_operation_to_proto(operation)),
            provider_replica: None,
            local_replica: None,
            local_storage: String::new(),
            build: Some(build_attempt_to_proto(build)),
        },
    }
}

fn public_v1_call_kind_from_proto(value: i32) -> Result<PublicV1CallKind, WireError> {
    Ok(
        match proto::PublicV1CallKind::try_from(value).map_err(|_| WireError::InvalidEnum {
            field: "public_v1.completion.call_kind",
            value,
        })? {
            proto::PublicV1CallKind::InitialPrimary => PublicV1CallKind::InitialPrimary,
            proto::PublicV1CallKind::FailoverPromotion => PublicV1CallKind::FailoverPromotion,
            proto::PublicV1CallKind::SecondaryEpochAdvance => {
                PublicV1CallKind::SecondaryEpochAdvance
            }
            proto::PublicV1CallKind::Open => PublicV1CallKind::Open,
            proto::PublicV1CallKind::Role => PublicV1CallKind::Role,
            proto::PublicV1CallKind::Epoch => PublicV1CallKind::Epoch,
            proto::PublicV1CallKind::Configuration => PublicV1CallKind::Configuration,
            proto::PublicV1CallKind::CatchUp => PublicV1CallKind::CatchUp,
            proto::PublicV1CallKind::Progress => PublicV1CallKind::Progress,
            proto::PublicV1CallKind::Swap => PublicV1CallKind::Swap,
            proto::PublicV1CallKind::Build => PublicV1CallKind::Build,
            proto::PublicV1CallKind::Remove => PublicV1CallKind::Remove,
            proto::PublicV1CallKind::Close => PublicV1CallKind::Close,
            proto::PublicV1CallKind::Abort => PublicV1CallKind::Abort,
            proto::PublicV1CallKind::Unknown => {
                return Err(WireError::InvalidEnum {
                    field: "public_v1.completion.call_kind",
                    value,
                });
            }
        },
    )
}

fn public_v1_call_kind_to_proto(value: PublicV1CallKind) -> proto::PublicV1CallKind {
    match value {
        PublicV1CallKind::InitialPrimary => proto::PublicV1CallKind::InitialPrimary,
        PublicV1CallKind::FailoverPromotion => proto::PublicV1CallKind::FailoverPromotion,
        PublicV1CallKind::SecondaryEpochAdvance => proto::PublicV1CallKind::SecondaryEpochAdvance,
        PublicV1CallKind::Open => proto::PublicV1CallKind::Open,
        PublicV1CallKind::Role => proto::PublicV1CallKind::Role,
        PublicV1CallKind::Epoch => proto::PublicV1CallKind::Epoch,
        PublicV1CallKind::Configuration => proto::PublicV1CallKind::Configuration,
        PublicV1CallKind::CatchUp => proto::PublicV1CallKind::CatchUp,
        PublicV1CallKind::Progress => proto::PublicV1CallKind::Progress,
        PublicV1CallKind::Swap => proto::PublicV1CallKind::Swap,
        PublicV1CallKind::Build => proto::PublicV1CallKind::Build,
        PublicV1CallKind::Remove => proto::PublicV1CallKind::Remove,
        PublicV1CallKind::Close => proto::PublicV1CallKind::Close,
        PublicV1CallKind::Abort => proto::PublicV1CallKind::Abort,
    }
}

fn public_v1_catch_up_mode_from_proto(value: i32) -> Result<PublicCatchUpMode, WireError> {
    match proto::PublicV1CatchUpMode::try_from(value).map_err(|_| WireError::InvalidEnum {
        field: "public_v1.completion.catch_up_mode",
        value,
    })? {
        proto::PublicV1CatchUpMode::WriteQuorum => Ok(PublicCatchUpMode::WriteQuorum),
        proto::PublicV1CatchUpMode::All => Ok(PublicCatchUpMode::All),
        proto::PublicV1CatchUpMode::Unknown => Err(WireError::InvalidEnum {
            field: "public_v1.completion.catch_up_mode",
            value,
        }),
    }
}

fn public_v1_catch_up_mode_to_proto(value: PublicCatchUpMode) -> proto::PublicV1CatchUpMode {
    match value {
        PublicCatchUpMode::WriteQuorum => proto::PublicV1CatchUpMode::WriteQuorum,
        PublicCatchUpMode::All => proto::PublicV1CatchUpMode::All,
    }
}

fn public_v1_completion_from_proto(
    value: proto::PublicV1OperationCompletion,
) -> Result<PublicV1OperationCompletion, WireError> {
    if value.input_digest_version != 1 {
        return Err(WireError::InvalidAuthority(
            "unsupported public-v1 operation-input digest version".into(),
        ));
    }
    let result_kind = proto::PublicV1OperationResultKind::try_from(value.result).map_err(|_| {
        WireError::InvalidEnum {
            field: "public_v1.completion.result",
            value: value.result,
        }
    })?;
    let result = match result_kind {
        proto::PublicV1OperationResultKind::PublicV1OperationResultSucceeded => {
            if !value.detail.is_empty() || value.rebuild_required.is_some() {
                return Err(WireError::InvalidAuthority(
                    "successful public-v1 completion contains failure detail".into(),
                ));
            }
            PublicV1OperationResult::Succeeded
        }
        proto::PublicV1OperationResultKind::PublicV1OperationResultFailed => {
            if value.detail.is_empty() || value.rebuild_required.is_some() {
                return Err(WireError::InvalidAuthority(
                    "failed public-v1 completion has a noncanonical payload".into(),
                ));
            }
            PublicV1OperationResult::Failed(value.detail)
        }
        proto::PublicV1OperationResultKind::PublicV1OperationResultAmbiguous => {
            if value.detail.is_empty() || value.rebuild_required.is_some() {
                return Err(WireError::InvalidAuthority(
                    "ambiguous public-v1 completion has a noncanonical payload".into(),
                ));
            }
            PublicV1OperationResult::Ambiguous(value.detail)
        }
        proto::PublicV1OperationResultKind::PublicV1OperationResultCancelled => {
            if !value.detail.is_empty() || value.rebuild_required.is_some() {
                return Err(WireError::InvalidAuthority(
                    "cancelled public-v1 completion contains failure detail".into(),
                ));
            }
            PublicV1OperationResult::Cancelled
        }
        proto::PublicV1OperationResultKind::PublicV1OperationResultRebuildRequired => {
            if !value.detail.is_empty() {
                return Err(WireError::InvalidAuthority(
                    "typed rebuild completion also contains text detail".into(),
                ));
            }
            PublicV1OperationResult::RebuildRequired(Box::new(rebuild_from_proto(
                value.rebuild_required.ok_or(WireError::MissingField(
                    "public_v1.completion.rebuild_required",
                ))?,
            )?))
        }
        proto::PublicV1OperationResultKind::PublicV1OperationResultUnknown => {
            return Err(WireError::InvalidEnum {
                field: "public_v1.completion.result",
                value: value.result,
            });
        }
    };
    Ok(PublicV1OperationCompletion {
        operation_id: OperationId::new(value.operation_id),
        revision: value.revision,
        call_kind: public_v1_call_kind_from_proto(value.call_kind)?,
        process_session_id: ProcessSessionId::new(value.process_session_id),
        input_digest: CanonicalOperationInputDigest::V1(CanonicalOperationInputDigestV1 {
            sha256: value.input_digest_sha256,
        }),
        replay_disposition: replay_from_proto(value.replay_disposition)?,
        history: public_v1_history_from_proto(
            value
                .history
                .ok_or(WireError::MissingField("public_v1.completion.history"))?,
        )?,
        storage_incarnation: public_v1_storage_from_proto(value.storage_incarnation.ok_or(
            WireError::MissingField("public_v1.completion.storage_incarnation"),
        )?)?,
        previous_configuration_id: (!value.previous_configuration_id.is_empty())
            .then(|| ConfigurationId::new(value.previous_configuration_id)),
        current_configuration_id: (!value.current_configuration_id.is_empty())
            .then(|| ConfigurationId::new(value.current_configuration_id)),
        catch_up_mode: value
            .catch_up_mode
            .map(public_v1_catch_up_mode_from_proto)
            .transpose()?,
        catch_up_specific_quorum: value.catch_up_specific_quorum,
        result,
    })
}

fn public_v1_completion_to_proto(
    value: &PublicV1OperationCompletion,
) -> proto::PublicV1OperationCompletion {
    let (result, detail, rebuild_required) = match &value.result {
        PublicV1OperationResult::Succeeded => (
            proto::PublicV1OperationResultKind::PublicV1OperationResultSucceeded,
            String::new(),
            None,
        ),
        PublicV1OperationResult::Failed(detail) => (
            proto::PublicV1OperationResultKind::PublicV1OperationResultFailed,
            detail.clone(),
            None,
        ),
        PublicV1OperationResult::Ambiguous(detail) => (
            proto::PublicV1OperationResultKind::PublicV1OperationResultAmbiguous,
            detail.clone(),
            None,
        ),
        PublicV1OperationResult::Cancelled => (
            proto::PublicV1OperationResultKind::PublicV1OperationResultCancelled,
            String::new(),
            None,
        ),
        PublicV1OperationResult::RebuildRequired(rebuild) => (
            proto::PublicV1OperationResultKind::PublicV1OperationResultRebuildRequired,
            String::new(),
            Some(rebuild_to_proto(rebuild)),
        ),
    };
    proto::PublicV1OperationCompletion {
        operation_id: value.operation_id.as_str().to_string(),
        revision: value.revision,
        call_kind: public_v1_call_kind_to_proto(value.call_kind) as i32,
        process_session_id: value.process_session_id.to_string(),
        input_digest_version: u32::from(value.input_digest.version()),
        input_digest_sha256: value.input_digest.sha256().to_string(),
        replay_disposition: replay_to_proto(value.replay_disposition) as i32,
        history: Some(public_v1_history_to_proto(&value.history)),
        storage_incarnation: Some(public_v1_storage_to_proto(&value.storage_incarnation)),
        previous_configuration_id: value
            .previous_configuration_id
            .as_ref()
            .map_or_else(String::new, ToString::to_string),
        current_configuration_id: value
            .current_configuration_id
            .as_ref()
            .map_or_else(String::new, ToString::to_string),
        catch_up_mode: value
            .catch_up_mode
            .map(|mode| public_v1_catch_up_mode_to_proto(mode) as i32),
        catch_up_specific_quorum: value.catch_up_specific_quorum,
        result: result as i32,
        detail,
        rebuild_required,
    }
}

/// Validates command fencing, policy, and PC/CC relationships.
pub fn validate_execute_request(request: &proto::ExecuteCommandRequest) -> Result<(), WireError> {
    ensure_supported_version(request.protocol_version)?;
    if request.resource_uid.is_empty() {
        return Err(WireError::MissingField("execute.resource_uid"));
    }
    if request.expected_process_session_id.is_empty() {
        return Err(WireError::MissingField(
            "execute.expected_process_session_id",
        ));
    }
    let target: ReplicaIdentity = request
        .target
        .clone()
        .ok_or(WireError::MissingField("execute.target"))?
        .try_into()?;
    match request
        .command
        .as_ref()
        .ok_or(WireError::MissingField("execute.command"))?
    {
        proto::execute_command_request::Command::InitializeAgentStore(command) => {
            for (field, value) in [
                (
                    "initialize.initialization_id",
                    command.initialization_id.as_str(),
                ),
                ("initialize.resource_uid", command.resource_uid.as_str()),
                (
                    "initialize.expected_instance_id",
                    command.expected_instance_id.as_str(),
                ),
                (
                    "initialize.expected_pod_uid",
                    command.expected_pod_uid.as_str(),
                ),
                (
                    "initialize.expected_pvc_uid",
                    command.expected_pvc_uid.as_str(),
                ),
                (
                    "initialize.assigned_agent_generation",
                    command.assigned_agent_generation.as_str(),
                ),
            ] {
                if value.is_empty() {
                    return Err(WireError::MissingField(field));
                }
            }
            if command.resource_uid != request.resource_uid
                || command.local_replica_id != target.replica_id.value()
                || command.expected_instance_id != target.instance_id.as_str()
                || command.assigned_agent_generation != target.agent_generation.as_str()
            {
                return Err(WireError::InvalidAuthority(
                    "initialization authority differs from execute envelope".into(),
                ));
            }
            if let Some(policy) = command.effective_policy.as_ref() {
                validate_policy(policy)?;
            } else {
                return Err(WireError::MissingField("initialize.effective_policy"));
            }
            if command.bootstrap_configuration.is_none() {
                return Err(WireError::MissingField(
                    "initialize.bootstrap_configuration",
                ));
            }
            Ok(())
        }
        proto::execute_command_request::Command::PublicV1History(command) => {
            let command = public_v1_history_command_from_proto(command.clone())?;
            if command.target() != &target
                || command.resource_uid().as_str() != request.resource_uid
            {
                return Err(WireError::InvalidAuthority(
                    "public-v1 history differs from execute envelope".into(),
                ));
            }
            Ok(())
        }
        proto::execute_command_request::Command::PublicV1Operation(command) => {
            let command = public_v1_operation_command_from_proto(command.clone())?;
            if command.target != target
                || command.intent.process_session_id.as_str() != request.expected_process_session_id
                || command.intent.identity.protocol_version != request.protocol_version
            {
                return Err(WireError::InvalidAuthority(
                    "public-v1 operation differs from execute envelope".into(),
                ));
            }
            Ok(())
        }
    }
}

pub fn normalize_execute_request(
    request: proto::ExecuteCommandRequest,
) -> Result<ExecuteEnvelope, WireError> {
    validate_execute_request(&request)?;
    let expected_process_session_id =
        ProcessSessionId::new(request.expected_process_session_id.clone());
    let target = request
        .target
        .ok_or(WireError::MissingField("execute.target"))?
        .try_into()?;
    let command = match request
        .command
        .ok_or(WireError::MissingField("execute.command"))?
    {
        proto::execute_command_request::Command::PublicV1History(command) => {
            ProtocolCommand::PublicV1History(Box::new(public_v1_history_command_from_proto(
                command,
            )?))
        }
        proto::execute_command_request::Command::PublicV1Operation(command) => {
            ProtocolCommand::PublicV1Operation(Box::new(public_v1_operation_command_from_proto(
                command,
            )?))
        }
        proto::execute_command_request::Command::InitializeAgentStore(command) => {
            ProtocolCommand::InitializeAgentStore(Box::new(InitializeAgentStore {
                initialization_id: InitializationId::new(command.initialization_id),
                resource_uid: ResourceUid::new(command.resource_uid),
                local_replica_id: ReplicaId::new(command.local_replica_id),
                expected_instance_id: ReplicaInstanceId::new(command.expected_instance_id),
                expected_pod_uid: PodUid::new(command.expected_pod_uid),
                expected_pvc_uid: PvcUid::new(command.expected_pvc_uid),
                assigned_agent_generation: AgentGeneration::new(command.assigned_agent_generation),
                effective_policy: policy_from_proto(
                    command
                        .effective_policy
                        .ok_or(WireError::MissingField("initialize.effective_policy"))?,
                )?,
                bootstrap_configuration: command
                    .bootstrap_configuration
                    .ok_or(WireError::MissingField(
                        "initialize.bootstrap_configuration",
                    ))?
                    .try_into()?,
                provisioning: command
                    .provisioning
                    .map(provisioning_from_proto)
                    .transpose()?,
            }))
        }
    };
    Ok(ExecuteEnvelope {
        resource_uid: ResourceUid::new(request.resource_uid),
        target,
        expected_process_session_id,
        command,
    })
}

impl From<Epoch> for proto::Epoch {
    fn from(value: Epoch) -> Self {
        Self {
            data_loss_number: value.data_loss_number,
            configuration_number: value.configuration_number,
        }
    }
}

impl From<proto::Epoch> for Epoch {
    fn from(value: proto::Epoch) -> Self {
        Self::new(value.data_loss_number, value.configuration_number)
    }
}

impl From<ReplicaIdentity> for proto::ReplicaIdentity {
    fn from(value: ReplicaIdentity) -> Self {
        Self {
            replica_id: value.replica_id.value(),
            instance_id: value.instance_id.to_string(),
            agent_generation: value.agent_generation.to_string(),
        }
    }
}

impl TryFrom<proto::ReplicaIdentity> for ReplicaIdentity {
    type Error = WireError;

    fn try_from(value: proto::ReplicaIdentity) -> Result<Self, Self::Error> {
        if value.replica_id <= 0 {
            return Err(WireError::InvalidAuthority(
                "replica ID must be positive".to_string(),
            ));
        }

        if value.instance_id.is_empty() {
            return Err(WireError::MissingField("replica_identity.instance_id"));
        }
        if value.agent_generation.is_empty() {
            return Err(WireError::MissingField("replica_identity.agent_generation"));
        }
        Ok(Self {
            replica_id: ReplicaId::new(value.replica_id),
            instance_id: ReplicaInstanceId::new(value.instance_id),
            agent_generation: AgentGeneration::new(value.agent_generation),
        })
    }
}

impl From<ConfigurationDescriptor> for proto::Configuration {
    fn from(value: ConfigurationDescriptor) -> Self {
        Self {
            configuration_id: value.configuration_id.to_string(),
            epoch: Some(value.epoch.into()),
            primary_id: value.primary_id.value(),
            members: value
                .members
                .into_iter()
                .map(|member| proto::ConfigurationMember {
                    identity: Some(member.identity.into()),
                    role: role_to_proto(member.role) as i32,
                })
                .collect(),
            write_quorum: value.write_quorum,
        }
    }
}

impl TryFrom<proto::Configuration> for ConfigurationDescriptor {
    type Error = WireError;

    fn try_from(value: proto::Configuration) -> Result<Self, Self::Error> {
        if value.configuration_id.is_empty() {
            return Err(WireError::MissingField("configuration.configuration_id"));
        }

        let epoch = value
            .epoch
            .ok_or(WireError::MissingField("configuration.epoch"))?
            .into();
        let mut members = value
            .members
            .into_iter()
            .map(|member| {
                let identity = member
                    .identity
                    .ok_or(WireError::MissingField("configuration.member.identity"))?
                    .try_into()?;
                let role = proto::ReplicaRole::try_from(member.role).map_err(|_| {
                    WireError::InvalidEnum {
                        field: "configuration.member.role",
                        value: member.role,
                    }
                })?;
                Ok(ConfigurationMember {
                    identity,
                    role: role_from_proto(role)?,
                })
            })
            .collect::<Result<Vec<_>, WireError>>()?;
        members.sort_by_key(|member| member.identity.replica_id);
        let configuration = ConfigurationDescriptor {
            configuration_id: ConfigurationId::new(value.configuration_id),
            epoch,
            primary_id: ReplicaId::new(value.primary_id),
            members,
            write_quorum: value.write_quorum,
        };
        validate_configuration(&configuration, None)
            .map_err(|error| WireError::InvalidAuthority(error.to_string()))?;
        Ok(configuration)
    }
}

impl From<BuildAuthority> for proto::BuildAuthority {
    fn from(authority: BuildAuthority) -> Self {
        Self {
            build_id: authority.build_id.to_string(),
            kind: match authority.kind {
                BuildAuthorityKind::Bootstrap => proto::BuildAuthorityKind::Bootstrap as i32,
                BuildAuthorityKind::Provisioning => proto::BuildAuthorityKind::Provisioning as i32,
                BuildAuthorityKind::Failover => proto::BuildAuthorityKind::Failover as i32,
            },
            source: Some(authority.source.into()),
            target: Some(authority.target.into()),
            current_configuration: Some(authority.current_configuration.into()),
            replication_boundary_lsn: authority.replication_boundary_lsn,
        }
    }
}

impl TryFrom<proto::BuildAuthority> for BuildAuthority {
    type Error = WireError;

    fn try_from(authority: proto::BuildAuthority) -> Result<Self, Self::Error> {
        build_authority_from_proto(authority)
    }
}

impl From<SwitchoverHandoff> for proto::SwitchoverHandoff {
    fn from(handoff: SwitchoverHandoff) -> Self {
        Self {
            preparation_generation: handoff.preparation_generation,
            preparation_operation_id: handoff.preparation_operation_id.to_string(),
            request_id: handoff.request_id.to_string(),
            source: Some(handoff.source.into()),
            target: Some(handoff.target.into()),
            starting_configuration_id: handoff.starting_configuration_id.to_string(),
            handoff_lsn: handoff.handoff_lsn,
            starting_epoch: Some(handoff.starting_epoch.into()),
        }
    }
}

impl From<crate::protocol::types::SwitchoverPreparationId> for proto::SwitchoverPreparationId {
    fn from(id: crate::protocol::types::SwitchoverPreparationId) -> Self {
        Self {
            operation_id: id.operation_id.to_string(),
            generation: id.generation,
        }
    }
}

impl TryFrom<proto::SwitchoverHandoff> for SwitchoverHandoff {
    type Error = WireError;

    fn try_from(handoff: proto::SwitchoverHandoff) -> Result<Self, Self::Error> {
        switchover_handoff_from_proto(handoff)
    }
}

fn switchover_handoff_from_proto(
    handoff: proto::SwitchoverHandoff,
) -> Result<SwitchoverHandoff, WireError> {
    if handoff.preparation_generation == 0
        || handoff.preparation_operation_id.is_empty()
        || handoff.request_id.is_empty()
        || handoff.starting_configuration_id.is_empty()
        || handoff.handoff_lsn < 0
    {
        return Err(WireError::InvalidAuthority(
            "planned switchover handoff contains invalid identifiers or progress".to_string(),
        ));
    }
    let handoff = SwitchoverHandoff {
        preparation_generation: handoff.preparation_generation,
        preparation_operation_id: OperationId::new(handoff.preparation_operation_id),
        request_id: SwitchoverRequestId::new(handoff.request_id),
        source: handoff
            .source
            .ok_or(WireError::MissingField("switchover_handoff.source"))?
            .try_into()?,
        target: handoff
            .target
            .ok_or(WireError::MissingField("switchover_handoff.target"))?
            .try_into()?,
        starting_configuration_id: ConfigurationId::new(handoff.starting_configuration_id),
        starting_epoch: handoff
            .starting_epoch
            .ok_or(WireError::MissingField("switchover_handoff.starting_epoch"))?
            .into(),
        handoff_lsn: handoff.handoff_lsn,
    };
    if handoff.source == handoff.target {
        return Err(WireError::InvalidAuthority(
            "planned switchover source and target must differ".to_string(),
        ));
    }
    Ok(handoff)
}

fn build_authority_from_proto(
    authority: proto::BuildAuthority,
) -> Result<BuildAuthority, WireError> {
    let kind = match proto::BuildAuthorityKind::try_from(authority.kind).map_err(|_| {
        WireError::InvalidEnum {
            field: "build_authority.kind",
            value: authority.kind,
        }
    })? {
        proto::BuildAuthorityKind::Bootstrap => BuildAuthorityKind::Bootstrap,
        proto::BuildAuthorityKind::Provisioning => BuildAuthorityKind::Provisioning,
        proto::BuildAuthorityKind::Failover => BuildAuthorityKind::Failover,
        proto::BuildAuthorityKind::Unspecified => {
            return Err(WireError::InvalidAuthority(
                "build authority kind is unspecified".to_string(),
            ));
        }
    };
    let authority = BuildAuthority {
        build_id: OperationId::new(authority.build_id),
        kind,
        source: authority
            .source
            .ok_or(WireError::MissingField("build_authority.source"))?
            .try_into()?,
        target: authority
            .target
            .ok_or(WireError::MissingField("build_authority.target"))?
            .try_into()?,
        current_configuration: authority
            .current_configuration
            .ok_or(WireError::MissingField(
                "build_authority.current_configuration",
            ))?
            .try_into()?,
        replication_boundary_lsn: authority.replication_boundary_lsn,
    };
    authority
        .validate()
        .map_err(|error| WireError::InvalidAuthority(error.to_string()))?;
    Ok(authority)
}

pub(crate) fn provisioning_from_proto(
    provisioning: proto::ProvisioningIntent,
) -> Result<ProvisioningIntent, WireError> {
    use proto::provisioning_intent::Purpose;
    let intent = ProvisioningIntent {
        purpose: match provisioning
            .purpose
            .ok_or(WireError::MissingField("provisioning.purpose"))?
        {
            Purpose::Replaces(replaces) => ProvisioningPurpose::replacement(replaces.try_into()?),
            Purpose::ScaleUp(scale_up) => ProvisioningPurpose::scale_up(scale_up.try_into()?),
        },
        pod_uid: PodUid::new(provisioning.pod_uid),
        pvc_uid: PvcUid::new(provisioning.pvc_uid),
        operation_id: OperationId::new(provisioning.operation_id),
    };
    if intent.operation_id.is_empty() || intent.pod_uid.is_empty() || intent.pvc_uid.is_empty() {
        return Err(WireError::InvalidAuthority(
            "provisioning identifiers must not be empty".to_string(),
        ));
    }
    if let Some(replaces) = intent.replacement()
        && (replaces.instance_id.is_empty() || replaces.agent_generation.is_empty())
    {
        return Err(WireError::InvalidAuthority(
            "replacement provisioning identifiers must not be empty".to_string(),
        ));
    }
    crate::protocol::validation::validate_scale_up_provisioning(&intent)
        .map_err(|error| WireError::InvalidAuthority(error.to_string()))?;
    Ok(intent)
}

pub(crate) fn provisioning_to_proto(provisioning: ProvisioningIntent) -> proto::ProvisioningIntent {
    use proto::provisioning_intent::Purpose;
    proto::ProvisioningIntent {
        purpose: Some(match provisioning.purpose.kind {
            ProvisioningKind::Replacement => Purpose::Replaces(
                provisioning
                    .purpose
                    .replaces
                    .expect("validated replacement provisioning")
                    .into(),
            ),
            ProvisioningKind::ScaleUp => Purpose::ScaleUp(
                provisioning
                    .purpose
                    .scale_up
                    .expect("validated scale-up provisioning")
                    .into(),
            ),
        }),
        pod_uid: provisioning.pod_uid.to_string(),
        pvc_uid: provisioning.pvc_uid.to_string(),
        operation_id: provisioning.operation_id.to_string(),
    }
}

pub(crate) fn role_to_proto(role: ReplicaRole) -> proto::ReplicaRole {
    match role {
        ReplicaRole::Primary => proto::ReplicaRole::Primary,
        ReplicaRole::ActiveSecondary => proto::ReplicaRole::ActiveSecondary,
        ReplicaRole::IdleSecondary => proto::ReplicaRole::IdleSecondary,
        ReplicaRole::None => proto::ReplicaRole::None,
    }
}

pub(crate) fn access_status_to_proto(status: AccessStatus) -> proto::AccessStatus {
    match status {
        AccessStatus::Granted => proto::AccessStatus::Granted,
        AccessStatus::ReconfigurationPending => proto::AccessStatus::ReconfigurationPending,
        AccessStatus::NotPrimary => proto::AccessStatus::NotPrimary,
        AccessStatus::NoWriteQuorum => proto::AccessStatus::NoWriteQuorum,
    }
}

pub(crate) fn role_from_proto(role: proto::ReplicaRole) -> Result<ReplicaRole, WireError> {
    match role {
        proto::ReplicaRole::Unknown => Err(WireError::InvalidEnum {
            field: "configuration.member.role",
            value: role as i32,
        }),
        proto::ReplicaRole::Primary => Ok(ReplicaRole::Primary),
        proto::ReplicaRole::ActiveSecondary => Ok(ReplicaRole::ActiveSecondary),
        proto::ReplicaRole::IdleSecondary => Ok(ReplicaRole::IdleSecondary),
        proto::ReplicaRole::None => Ok(ReplicaRole::None),
    }
}

pub(crate) fn access_status_from_proto(
    status: proto::AccessStatus,
) -> Result<AccessStatus, WireError> {
    match status {
        proto::AccessStatus::Unknown => Err(WireError::InvalidEnum {
            field: "agent_status.write_status",
            value: status as i32,
        }),
        proto::AccessStatus::Granted => Ok(AccessStatus::Granted),
        proto::AccessStatus::ReconfigurationPending => Ok(AccessStatus::ReconfigurationPending),
        proto::AccessStatus::NotPrimary => Ok(AccessStatus::NotPrimary),
        proto::AccessStatus::NoWriteQuorum => Ok(AccessStatus::NoWriteQuorum),
    }
}

fn validate_policy(policy: &proto::EffectivePolicy) -> Result<(), WireError> {
    let expected = EffectivePolicy::fixed(policy.replica_set_size, policy.failover_delay_seconds)
        .ok_or_else(|| {
        WireError::InvalidAuthority("replica-set size must be positive".to_string())
    })?;
    if policy.write_quorum != expected.write_quorum || policy.read_quorum != expected.read_quorum {
        return Err(WireError::InvalidAuthority(
            "effective policy quorum values are not fixed-majority values".to_string(),
        ));
    }
    Ok(())
}

pub(crate) fn policy_from_proto(
    policy: proto::EffectivePolicy,
) -> Result<EffectivePolicy, WireError> {
    validate_policy(&policy)?;
    Ok(EffectivePolicy {
        replica_set_size: policy.replica_set_size,
        write_quorum: policy.write_quorum,
        read_quorum: policy.read_quorum,
        failover_delay_seconds: policy.failover_delay_seconds,
    })
}
