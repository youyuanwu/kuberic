use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};

use super::*;
use crate::authority::BuildAuthorityKind;

#[derive(Default)]
struct Control {
    aborts: AtomicUsize,
}

#[test]
fn legacy_build_progress_does_not_weaken_public_v1_idle_validation() {
    let identity = ReplicaIdentity {
        replica_id: ReplicaId::new(2),
        instance_id: crate::protocol::types::ReplicaInstanceId::new("legacy-build"),
        agent_generation: crate::protocol::types::AgentGeneration::new("legacy-generation"),
    };
    let mut legacy = ReplicaInformation::new(
        OperationId::new("legacy-build"),
        identity,
        "legacy://target".into(),
    );
    legacy.current_progress = 0;
    legacy.catch_up_capability = 0;

    legacy.validate_progress_values().unwrap();
    assert!(legacy.validate_public_progress().is_err());
}

#[test]
fn dropped_configuration_preparation_invalidates_native_generation_synchronously() {
    let identity = ReplicaIdentity {
        replica_id: ReplicaId::new(1),
        instance_id: crate::protocol::types::ReplicaInstanceId::new("prepared-drop"),
        agent_generation: crate::protocol::types::AgentGeneration::new("prepared-drop-generation"),
    };
    let configuration = ManagedReplicaConfiguration {
        local_identity: identity.clone(),
        previous_configuration: None,
        current_configuration: ConfigurationDescriptor::new(
            Epoch::new(0, 1),
            identity.replica_id,
            vec![crate::protocol::types::ConfigurationMember {
                identity,
                role: ReplicaRole::Primary,
            }],
            1,
        ),
        switchover_handoff: None,
        secondary_removal: None,
        scale_up: None,
        build_kind: BuildAuthorityKind::Provisioning,
    };
    let active = Arc::new(AtomicBool::new(true));
    let native_generation = Arc::new(AtomicU64::new(7));
    let preparation = ManagedConfigurationPreparation {
        fence: ManagedOperationFence {
            configuration: Some(configuration.clone()),
            engine_session_id: "prepared-drop-session".into(),
            engine_generation: 7,
        },
        configuration,
        host_generation: 3,
        active: active.clone(),
        native_generation: native_generation.clone(),
    };
    drop(preparation);
    assert!(!active.load(Ordering::Acquire));
    assert_eq!(native_generation.load(Ordering::Acquire), 8);
}

#[async_trait]
impl Replicator for Control {
    async fn open(&self) -> Result<String> {
        Ok(String::new())
    }

    async fn change_role(&self, _: Epoch, _: ReplicaRole) -> Result<()> {
        Ok(())
    }

    async fn update_epoch(&self, _: Epoch) -> Result<()> {
        Ok(())
    }

    async fn close(&self) -> Result<()> {
        Ok(())
    }

    fn abort(&self) {
        self.aborts.fetch_add(1, Ordering::SeqCst);
    }

    async fn current_progress(&self) -> Result<Lsn> {
        Ok(0)
    }

    async fn catch_up_capability(&self) -> Result<Lsn> {
        Ok(0)
    }
}

#[async_trait]
impl PrimaryReplicator for Control {
    async fn on_data_loss(&self) -> Result<bool> {
        Ok(false)
    }

    async fn update_catch_up_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
        _: ReplicaSetConfiguration,
    ) -> Result<()> {
        Ok(())
    }

    async fn wait_for_catch_up_quorum(&self, _: ReplicaSetQuorumMode) -> Result<()> {
        Ok(())
    }

    async fn update_current_replica_set_configuration(
        &self,
        _: ReplicaSetConfiguration,
    ) -> Result<()> {
        Ok(())
    }

    async fn build_replica(&self, _: ReplicaInformation) -> Result<()> {
        Ok(())
    }

    async fn remove_replica(&self, _: ReplicaId) -> Result<()> {
        Ok(())
    }
}

#[test]
fn public_capabilities_are_immutable_across_wrapping_and_failed_attachment() {
    let capabilities = ReplicatorCapabilities {
        catch_up_specific_quorum: true,
        data_loss_replay: DataLossReplay::Convergent,
    };
    let original = Arc::new(Control::default());
    let interfaces = ReplicatorInterfaces::primary(original, None, capabilities);
    assert_eq!(interfaces.capabilities(), capabilities);
    let wrapped = interfaces.wrap_primary(Arc::new(Control::default()), None);
    assert_eq!(wrapped.capabilities(), capabilities);
    let reservation = ReplicatorCreationReservation::new(RuntimeHostToken::new());
    let attachment = wrapped.prepare_attachment(reservation).unwrap();
    assert_eq!(
        attachment.capabilities(RuntimeHostToken::new()),
        capabilities
    );
    assert_eq!(wrapped.capabilities(), capabilities);
    let other = ReplicatorCreationReservation::new(RuntimeHostToken::new());
    assert!(wrapped.prepare_attachment(other).is_err());
    assert_eq!(wrapped.capabilities(), capabilities);
}

#[test]
fn attachment_retries_bind_only_the_original_creation_reservation() {
    let control = Arc::new(Control::default());
    let interfaces =
        ReplicatorInterfaces::secondary(control, None, ReplicatorCapabilities::conservative());
    let reservation = ReplicatorCreationReservation::new(RuntimeHostToken::new());
    let attachment = interfaces.prepare_attachment(reservation).unwrap();
    let retry = interfaces.prepare_attachment(reservation).unwrap();
    assert_eq!(
        attachment.identity(RuntimeHostToken::new()),
        retry.identity(RuntimeHostToken::new()),
    );
    let other = ReplicatorCreationReservation::new(RuntimeHostToken::new());
    assert_ne!(
        reservation.identity(RuntimeHostToken::new()),
        other.identity(RuntimeHostToken::new()),
    );
    assert!(interfaces.prepare_attachment(other).is_err());
    assert_eq!(
        interfaces
            .prepare_attachment(reservation)
            .unwrap()
            .identity(RuntimeHostToken::new()),
        reservation.identity(RuntimeHostToken::new()),
    );
}

#[test]
fn unregistered_capability_bundle_aborts_once_after_its_last_handle_drops() {
    let control = Arc::new(Control::default());
    let interfaces = ReplicatorInterfaces::secondary(
        control.clone(),
        None,
        ReplicatorCapabilities::conservative(),
    );
    let reservation = ReplicatorCreationReservation::new(RuntimeHostToken::new());
    let attachment = interfaces.prepare_attachment(reservation).unwrap();
    drop(interfaces);
    assert_eq!(control.aborts.load(Ordering::SeqCst), 0);
    drop(attachment);
    assert_eq!(control.aborts.load(Ordering::SeqCst), 1);
}

#[test]
fn successful_host_registration_disarms_bundle_cleanup() {
    let control = Arc::new(Control::default());
    let interfaces = ReplicatorInterfaces::secondary(
        control.clone(),
        None,
        ReplicatorCapabilities::conservative(),
    );
    let reservation = ReplicatorCreationReservation::new(RuntimeHostToken::new());
    let attachment = interfaces.prepare_attachment(reservation).unwrap();
    attachment.disarm();
    drop(attachment);
    drop(interfaces);
    assert_eq!(control.aborts.load(Ordering::SeqCst), 0);
}
