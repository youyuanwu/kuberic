use std::sync::Mutex;

use kuberic_controller::cluster_api::PublicV1Scope;
use kuberic_controller::evaluator::testing::{
    PublicV1SessionRenewalApi, PublicV1SessionRenewalPlan, accept_public_v1_session_access_closed,
    accept_public_v1_session_epoch_updated, accept_public_v1_session_renewal,
    accept_public_v1_survivor_retired, execute_public_v1_session_renewal,
    plan_public_v1_session_renewal,
};
use kuberic_runtime::protocol::command::{
    ProtocolCommand, PublicV1HistoryCommand, PublicV1SessionRenewalCommand,
};
use kuberic_runtime::protocol::public_operations::{
    ProcessSessionRenewalRequired, PublicProgress, PublicProgressRole, PublicSessionRenewalStage,
    PublicV1AgentReport, PublicV1Identity, PublicV1SessionRenewalStatus,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationDescriptor, ConfigurationMember, Epoch,
    OperationId, PodUid, ProcessSessionEndpoint, ProcessSessionId, ProcessSessionRenewalReason,
    PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
    ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ReplicatorCreationId, ResourceUid,
    StorageGeneration, derive_bootstrap_history, derive_history_initialization_id,
};

#[derive(Default)]
struct FakeApi {
    status: Mutex<Option<PublicV1SessionRenewalStatus>>,
    dispatched: Mutex<Vec<ProtocolCommand>>,
}

#[async_trait::async_trait]
impl PublicV1SessionRenewalApi for FakeApi {
    async fn observe_public_v1_session_renewal(
        &self,
        _scope: &PublicV1Scope,
    ) -> Result<Option<PublicV1SessionRenewalStatus>, String> {
        Ok(self.status.lock().unwrap().clone())
    }

    async fn persist_public_v1_session_renewal(
        &self,
        _scope: &PublicV1Scope,
        expected: Option<&PublicV1SessionRenewalStatus>,
        next: &PublicV1SessionRenewalStatus,
    ) -> Result<(), String> {
        let mut status = self.status.lock().unwrap();
        if status.as_ref() != expected {
            return Err("conditional session-renewal write was stale".into());
        }
        *status = Some(next.clone());
        Ok(())
    }

    async fn dispatch_public_v1_session_renewal(
        &self,
        _scope: &PublicV1Scope,
        expected: &PublicV1SessionRenewalStatus,
        command: &ProtocolCommand,
    ) -> Result<(), String> {
        if self.status.lock().unwrap().as_ref() != Some(expected) {
            return Err("session-renewal dispatch was stale".into());
        }
        self.dispatched.lock().unwrap().push(command.clone());
        Ok(())
    }
}

fn test_scope() -> PublicV1Scope {
    PublicV1Scope {
        namespace: "kuberic-system".into(),
        name: "resource-1".into(),
        resource_uid: ResourceUid::new("resource-1"),
    }
}

fn replica(id: i64) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
        agent_generation: AgentGeneration::new(format!("generation-{id}")),
    }
}

fn endpoint(id: i64, session: &str, role: ReplicaRole) -> ProcessSessionEndpoint {
    let identity = replica(id);
    ProcessSessionEndpoint {
        storage_incarnation: ReplicaStorageIncarnationContext {
            replica: identity.clone(),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new(format!("pod-{id}")),
                pvc_uid: PvcUid::new(format!("pvc-{id}")),
            },
            storage_generation: StorageGeneration::new(format!("storage-{id}")),
        },
        identity,
        process_session_id: ProcessSessionId::new(session),
        replicator_creation_id: ReplicatorCreationId::new(format!("creation-{session}")),
        role,
    }
}

fn configuration(
    epoch: Epoch,
    primary: &ProcessSessionEndpoint,
    secondary: Option<&ProcessSessionEndpoint>,
) -> ConfigurationDescriptor {
    let mut members = vec![ConfigurationMember {
        identity: primary.identity.clone(),
        role: ReplicaRole::Primary,
    }];
    if let Some(secondary) = secondary {
        members.push(ConfigurationMember {
            identity: secondary.identity.clone(),
            role: ReplicaRole::ActiveSecondary,
        });
    }
    ConfigurationDescriptor::new(epoch, primary.identity.replica_id, members, 1)
}

fn requirement(reason: ProcessSessionRenewalReason) -> ProcessSessionRenewalRequired {
    let resource_uid = ResourceUid::new("resource-1");
    let generation = PublicProtocolGeneration::new(42);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    let history = derive_bootstrap_history(&resource_uid, generation, &initialization);
    let previous_epoch = Epoch::new(0, 10);
    let renewal_epoch = Epoch::new(0, 11);
    let primary = endpoint(1, "primary", ReplicaRole::Primary);
    let secondary = endpoint(2, "secondary", ReplicaRole::ActiveSecondary);
    let (predecessor, replacement, survivors, fresh_configuration) = match reason {
        ProcessSessionRenewalReason::PrimaryReplacement => {
            let replacement = endpoint(1, "primary-new", ReplicaRole::None);
            (
                primary.clone(),
                Some(replacement),
                vec![secondary.clone()],
                configuration(renewal_epoch, &primary, Some(&secondary)),
            )
        }
        ProcessSessionRenewalReason::ActiveSecondaryReplacement => {
            let replacement = endpoint(2, "secondary-new", ReplicaRole::None);
            (
                secondary,
                Some(replacement),
                vec![primary.clone()],
                configuration(
                    renewal_epoch,
                    &primary,
                    Some(&endpoint(2, "secondary", ReplicaRole::ActiveSecondary)),
                ),
            )
        }
        ProcessSessionRenewalReason::PeerWithdrawal => (
            secondary,
            None,
            vec![primary.clone()],
            configuration(renewal_epoch, &primary, None),
        ),
        ProcessSessionRenewalReason::DataLossAttempt => {
            panic!("controller ordering fixture does not use data loss")
        }
    };
    ProcessSessionRenewalRequired {
        operation_id: OperationId::new("session-renewal"),
        reason,
        resource_uid,
        history,
        previous_epoch,
        renewal_epoch,
        predecessor,
        replacement,
        surviving_sessions: survivors,
        fresh_configuration,
        predecessor_contained: true,
        data_loss: None,
    }
}

fn replacement_report(required: &ProcessSessionRenewalRequired) -> PublicV1AgentReport {
    let replacement = required.replacement.as_ref().unwrap();
    PublicV1AgentReport {
        identity: PublicV1Identity::new(42),
        resource_uid: required.resource_uid.clone(),
        replica: replacement.identity.clone(),
        process_session_id: replacement.process_session_id.clone(),
        replicator_creation_id: replacement.replicator_creation_id.clone(),
        report_sequence: 1,
        open_mode: kuberic_runtime::application::OpenMode::Existing,
        role: ReplicaRole::None,
        read_status: AccessStatus::NotPrimary,
        write_status: AccessStatus::NotPrimary,
        history: required.history.clone(),
        storage_incarnation: replacement.storage_incarnation.clone(),
        progress: PublicProgress {
            role: PublicProgressRole::Recovery,
            first: 9,
            last: 9,
        },
        replication_address: None,
        service_location: None,
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    }
}

fn pending_barriers(required: ProcessSessionRenewalRequired) -> PublicV1SessionRenewalStatus {
    PublicV1SessionRenewalStatus::required(required)
        .unwrap()
        .close_access()
        .unwrap()
        .freeze_surviving_set()
        .unwrap()
        .begin_epoch_barriers()
        .unwrap()
}

fn renewal_command(plan: &PublicV1SessionRenewalPlan) -> &PublicV1SessionRenewalCommand {
    let PublicV1SessionRenewalPlan::Dispatch { command, .. } = plan else {
        panic!("expected dispatch plan");
    };
    let ProtocolCommand::PublicV1History(command) = command else {
        panic!("expected dormant public-v1 command");
    };
    let PublicV1HistoryCommand::SessionRenewal { command } = command.as_ref() else {
        panic!("expected session-renewal command");
    };
    command
}

#[tokio::test]
async fn public_v1_session_renewal_is_conditionally_accepted() {
    let required = requirement(ProcessSessionRenewalReason::PrimaryReplacement);
    let report = replacement_report(&required);
    let plan = accept_public_v1_session_renewal(None, required.clone(), Some(&report)).unwrap();
    let api = FakeApi::default();
    execute_public_v1_session_renewal(&api, &test_scope(), plan.clone())
        .await
        .unwrap();
    assert_eq!(
        api.status.lock().unwrap().as_ref().unwrap().required,
        required
    );

    let stale = FakeApi {
        status: Mutex::new(Some(
            PublicV1SessionRenewalStatus::required(requirement(
                ProcessSessionRenewalReason::ActiveSecondaryReplacement,
            ))
            .unwrap(),
        )),
        ..Default::default()
    };
    assert!(
        execute_public_v1_session_renewal(&stale, &test_scope(), plan)
            .await
            .is_err()
    );
}

#[test]
fn public_v1_primary_and_secondary_replacements_obey_epoch_ordering() {
    for reason in [
        ProcessSessionRenewalReason::PrimaryReplacement,
        ProcessSessionRenewalReason::ActiveSecondaryReplacement,
    ] {
        let required = requirement(reason);
        let pending = pending_barriers(required.clone());
        let plan = plan_public_v1_session_renewal(&pending, &[]).unwrap();
        assert!(matches!(
            renewal_command(&plan),
            PublicV1SessionRenewalCommand::UpdateEpochBarrier { epoch, .. }
                if *epoch == required.renewal_epoch
        ));
        assert!(
            pending
                .replay_replacement_role(
                    &required.replacement.as_ref().unwrap().process_session_id,
                    required.renewal_epoch,
                    required.predecessor.role,
                )
                .is_err()
        );

        let updated = accept_public_v1_session_epoch_updated(
            &pending,
            &required.surviving_sessions[0],
            required.renewal_epoch,
        )
        .unwrap();
        let role_pending = updated.finish_epoch_barriers().unwrap();
        assert_eq!(
            role_pending.stage,
            PublicSessionRenewalStage::ReplacementRolePending
        );
        assert!(matches!(
            renewal_command(&plan_public_v1_session_renewal(&role_pending, &[]).unwrap()),
            PublicV1SessionRenewalCommand::ReplayReplacementRole { epoch, role, .. }
                if *epoch == required.renewal_epoch && *role == required.predecessor.role
        ));
    }
}

#[tokio::test]
async fn hosted_public_v1_unreachable_survivor_blocks_access_until_return_or_retirement() {
    tokio::time::timeout(std::time::Duration::from_secs(270), async {
        let required = requirement(ProcessSessionRenewalReason::PrimaryReplacement);
        let pending = pending_barriers(required.clone());
        assert!(matches!(
            plan_public_v1_session_renewal(
                &pending,
                &[required.surviving_sessions[0].process_session_id.clone()],
            )
            .unwrap(),
            PublicV1SessionRenewalPlan::WaitForSurvivor { survivor, .. }
                if *survivor == required.surviving_sessions[0]
        ));

        let mut excluding = required;
        excluding.fresh_configuration =
            configuration(excluding.renewal_epoch, &excluding.predecessor, None);
        let pending = pending_barriers(excluding.clone());
        let excluded = accept_public_v1_survivor_retired(
            &pending,
            &excluding.surviving_sessions[0],
            OperationId::new("retire-unreachable-survivor"),
        )
        .unwrap();
        assert_eq!(
            excluded.finish_epoch_barriers().unwrap().stage,
            PublicSessionRenewalStage::ReplacementRolePending
        );

        let pending =
            pending_barriers(requirement(ProcessSessionRenewalReason::PrimaryReplacement));
        let returned = accept_public_v1_session_epoch_updated(
            &pending,
            &pending.required.surviving_sessions[0],
            pending.required.renewal_epoch,
        )
        .unwrap();
        assert_eq!(
            returned.finish_epoch_barriers().unwrap().stage,
            PublicSessionRenewalStage::ReplacementRolePending
        );
    })
    .await
    .unwrap();
}

#[test]
fn public_v1_peer_withdrawal_advances_epoch_without_role_replay() {
    let required = requirement(ProcessSessionRenewalReason::PeerWithdrawal);
    required.validate().unwrap();
    let accepted = PublicV1SessionRenewalStatus::required(required.clone()).unwrap();
    let close = plan_public_v1_session_renewal(&accepted, &[]).unwrap();
    assert!(matches!(
        renewal_command(&close),
        PublicV1SessionRenewalCommand::CloseAccessAndIngress { .. }
    ));
    assert_eq!(
        renewal_command(&close).expected_endpoint(),
        &required.predecessor
    );
    let pending = pending_barriers(required.clone());
    let updated = accept_public_v1_session_epoch_updated(
        &pending,
        &required.surviving_sessions[0],
        required.renewal_epoch,
    )
    .unwrap();
    let configuration_pending = updated.finish_epoch_barriers().unwrap();
    assert_eq!(
        configuration_pending.stage,
        PublicSessionRenewalStage::ConfigurationPending
    );
    assert!(matches!(
        renewal_command(
            &plan_public_v1_session_renewal(&configuration_pending, &[]).unwrap()
        ),
        PublicV1SessionRenewalCommand::InstallFreshConfiguration { configuration, .. }
            if configuration.epoch == required.renewal_epoch
    ));
}

#[test]
fn public_v1_session_renewal_rejects_predecessor_first_authority() {
    let mut required = requirement(ProcessSessionRenewalReason::PrimaryReplacement);
    required.predecessor_contained = false;
    let report = replacement_report(&required);
    assert!(accept_public_v1_session_renewal(None, required, Some(&report)).is_err());
}

#[test]
fn public_v1_session_renewal_rejects_same_epoch_substitution() {
    let mut required = requirement(ProcessSessionRenewalReason::PrimaryReplacement);
    required.renewal_epoch = required.previous_epoch;
    required.fresh_configuration = configuration(
        required.renewal_epoch,
        &required.predecessor,
        Some(&required.surviving_sessions[0]),
    );
    assert!(required.validate().is_err());

    let accepted = PublicV1SessionRenewalStatus::required(requirement(
        ProcessSessionRenewalReason::PrimaryReplacement,
    ))
    .unwrap();
    let closed = accept_public_v1_session_access_closed(&accepted).unwrap();
    assert_eq!(closed.stage, PublicSessionRenewalStage::AccessClosed);
}
