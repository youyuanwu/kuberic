//! Application-owned companion for the local controller authority protocol.

use super::*;
use crate::protocol::public_operations::{PublicConfiguration, PublicInboundBuildAuthority};
use tokio::sync::{Mutex, watch};

/// Optional controller integration for an independent native engine.
///
/// Installation/revocation only concern the exact copy attempt, not voting
/// admission or database receipts. The host retains these calls when an RPC
/// observer disappears. Implementations validate host authority before native
/// effects, after awaits and before publishing completion. Successful drainage
/// promises actual joining of owned descendants; an error never proves joining.
#[async_trait]
pub trait CustomReplicatorControl: Send + Sync {
    async fn install_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()>;
    async fn revoke_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()>;
    /// Observe actual idle peers coherently with this last installed host
    /// configuration. Return an error if unavailable or mismatched. A secondary
    /// host configuration is control authority, not primary native admission.
    async fn idle_replicas(&self, configuration: &PublicConfiguration) -> Result<Vec<ReplicaId>>;
    /// Invalidate native ingress/access and join all owned native work.
    async fn shutdown_and_drain(&self) -> Result<()>;
}

struct Owner {
    context: ReplicatorFactoryContext,
    application: Arc<dyn CustomReplicatorControl>,
    serial: Mutex<Option<PublicInboundBuildAuthority>>,
    stopped: AtomicBool,
    revoked: AtomicBool,
    contained: watch::Sender<bool>,
}

pub(super) struct CustomControl(Arc<Owner>);

impl CustomControl {
    pub(super) fn new(
        context: ReplicatorFactoryContext,
        application: Arc<dyn CustomReplicatorControl>,
    ) -> Self {
        let (contained, _) = watch::channel(false);
        Self(Arc::new(Owner {
            context,
            application,
            serial: Mutex::new(None),
            stopped: AtomicBool::new(false),
            revoked: AtomicBool::new(false),
            contained,
        }))
    }
}

impl Owner {
    async fn scope(&self, authority: &PublicInboundBuildAuthority) -> Result<()> {
        if self.stopped.load(Ordering::Acquire)
            || authority.attempt.target != self.context.identity
            || authority.attempt.target_session != self.context.process_session_id
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.context
            .access
            .validate_custom_build_authority(
                self.context
                    .reservation
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?,
                authority,
                false,
            )
            .await
    }

    async fn fail_closed(&self) -> Result<()> {
        self.stopped.store(true, Ordering::Release);
        let invalidation = self
            .context
            .revoke_replacement_inbound_build_authority()
            .await;
        let drainage = self.application.shutdown_and_drain().await;
        invalidation?;
        drainage?;
        self.contained.send_replace(true);
        Ok(())
    }
}

struct ActionGuard(Arc<Owner>, bool);
impl Drop for ActionGuard {
    fn drop(&mut self) {
        if self.1 {
            self.0.stopped.store(true, Ordering::Release);
            let owner = self.0.clone();
            tokio::spawn(async move {
                let _serial = owner.serial.lock().await;
                if let Err(error) = owner.fail_closed().await {
                    tracing::error!(%error, "lost custom action containment failed");
                }
            });
        }
    }
}

async fn retained(
    action: impl std::future::Future<Output = Result<()>> + Send + 'static,
) -> Result<()> {
    // The task owns the action independently of the caller's response waiter.
    tokio::spawn(action).await.map_err(|error| {
        RuntimeError::Application(format!("custom controller owner lost: {error}"))
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{public_operations::*, types::*};
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::Semaphore;

    // Contract/ownership fixture only. This is not a native engine witness.
    struct Fixture {
        expected: PublicInboundBuildAuthority,
        durable: StdMutex<Option<PublicInboundBuildAuthority>>,
        generation: StdMutex<u64>,
        installs: AtomicUsize,
        revokes: AtomicUsize,
        fail_drain: AtomicBool,
        fail_install: AtomicBool,
        panic_install: AtomicBool,
        panic_drain: AtomicBool,
        entered: Semaphore,
        release: Semaphore,
        hold: AtomicBool,
    }
    #[async_trait]
    impl PartitionAccessView for Fixture {
        fn partition_information(&self) -> PartitionInformation {
            PartitionInformation {
                partition_id: PartitionId::new("contract"),
            }
        }
        async fn read_status(&self) -> Result<AccessStatus> {
            Ok(AccessStatus::NotPrimary)
        }
        async fn write_status(&self) -> Result<AccessStatus> {
            Ok(AccessStatus::NotPrimary)
        }
        async fn validate_custom_build_authority(
            &self,
            _: ReplicatorCreationReservation,
            authority: &PublicInboundBuildAuthority,
            installed: bool,
        ) -> Result<()> {
            if authority != &self.expected
                || (installed && self.durable.lock().unwrap().as_ref() != Some(authority))
            {
                Err(RuntimeError::AuthorityNotAdmitted)
            } else {
                Ok(())
            }
        }
        async fn classify_replacement_inbound_build_authority(
            &self,
            a: &PublicInboundBuildAuthority,
        ) -> Result<PublicInboundBuildAdmission> {
            if self.durable.lock().unwrap().as_ref() == Some(a) {
                Ok(PublicInboundBuildAdmission::Current)
            } else if a.operation_revision > *self.generation.lock().unwrap() {
                Ok(PublicInboundBuildAdmission::Successor)
            } else {
                Err(RuntimeError::AuthorityNotAdmitted)
            }
        }
        async fn install_replacement_inbound_build_authority(
            &self,
            a: &PublicInboundBuildAuthority,
        ) -> Result<()> {
            *self.generation.lock().unwrap() = a.operation_revision;
            *self.durable.lock().unwrap() = Some(a.clone());
            Ok(())
        }
        async fn revoke_replacement_inbound_build_authority(&self) -> Result<()> {
            *self.durable.lock().unwrap() = None;
            Ok(())
        }
        fn release_provider_writer(&self) {}
        async fn report_load(&self, _: Vec<LoadMetric>) -> Result<()> {
            Ok(())
        }
        async fn report_fault(&self, _: FaultType) -> Result<()> {
            Ok(())
        }
    }
    #[async_trait]
    impl CustomReplicatorControl for Fixture {
        async fn install_inbound_build_authority(
            &self,
            _: &PublicInboundBuildAuthority,
        ) -> Result<()> {
            self.installs.fetch_add(1, Ordering::SeqCst);
            self.entered.add_permits(1);
            assert!(
                !self.panic_install.load(Ordering::SeqCst),
                "lost native installer"
            );
            if self.hold.load(Ordering::SeqCst) {
                self.release.acquire().await.unwrap().forget();
            }
            if self.fail_install.load(Ordering::SeqCst) {
                Err(RuntimeError::Application(
                    "partial native installation".into(),
                ))
            } else {
                Ok(())
            }
        }
        async fn revoke_inbound_build_authority(
            &self,
            _: &PublicInboundBuildAuthority,
        ) -> Result<()> {
            self.revokes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn idle_replicas(&self, _: &PublicConfiguration) -> Result<Vec<ReplicaId>> {
            Err(RuntimeError::ProgressNotReady)
        }
        async fn shutdown_and_drain(&self) -> Result<()> {
            assert!(!self.panic_drain.load(Ordering::SeqCst), "lost drain owner");
            if self.fail_drain.load(Ordering::SeqCst) {
                Err(RuntimeError::Application("descendant remains".into()))
            } else {
                Ok(())
            }
        }
    }
    fn fixture() -> (
        Arc<CustomControl>,
        Arc<Fixture>,
        PublicInboundBuildAuthority,
    ) {
        let identity = |id| ReplicaIdentity {
            replica_id: ReplicaId::new(id),
            instance_id: ReplicaInstanceId::new(format!("i{id}")),
            agent_generation: AgentGeneration::new(format!("g{id}")),
        };
        let storage = |id| ReplicaStorageIncarnationContext {
            replica: identity(id),
            storage: ReplicaStorageIdentity::Volatile {
                identity: format!("s{id}"),
            },
            storage_generation: StorageGeneration::new(format!("sg{id}")),
        };
        let authority = PublicInboundBuildAuthority {
            operation_id: OperationId::new("op"),
            operation_revision: 1,
            attempt: PublicBuildAttemptReference {
                resource_uid: ResourceUid::new("resource"),
                epoch: Epoch::new(0, 1),
                build_id: OperationId::new("build"),
                source: identity(1),
                source_storage: storage(1),
                source_session: ProcessSessionId::new("p1"),
                target: identity(2),
                target_storage: storage(2),
                target_session: ProcessSessionId::new("p2"),
            },
            history: HistoryContext {
                resource_uid: ResourceUid::new("resource"),
                protocol_generation: PublicProtocolGeneration::new(1),
                history_id: HistoryId::new("history"),
                data_loss_number: 0,
            },
            current_configuration_id: ConfigurationId::new("config"),
            source_address: "source".into(),
            target_address: "target".into(),
        };
        let fixture = Arc::new(Fixture {
            expected: authority.clone(),
            durable: StdMutex::new(None),
            generation: StdMutex::new(0),
            installs: AtomicUsize::new(0),
            revokes: AtomicUsize::new(0),
            fail_drain: AtomicBool::new(false),
            fail_install: AtomicBool::new(false),
            panic_install: AtomicBool::new(false),
            panic_drain: AtomicBool::new(false),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
            hold: AtomicBool::new(false),
        });
        let context = ReplicatorFactoryContext::new(
            RuntimeHostToken::new(),
            identity(2),
            ProcessSessionId::new("p2"),
            fixture.clone(),
            PublicV1LifecycleBinding {
                identity: PublicV1Identity::new(1),
                resource_uid: ResourceUid::new("resource"),
                spec_generation: 1,
                state_persistence: StatePersistence::Volatile,
            },
            "127.0.0.1:1".parse().unwrap(),
        )
        .for_creation(ReplicatorCreationReservation::new(RuntimeHostToken::new()));
        (
            Arc::new(CustomControl::new(context, fixture.clone())),
            fixture,
            authority,
        )
    }

    #[tokio::test]
    async fn exact_replay_revoked_replay_and_stale_revoke_are_distinct() {
        let (control, f, a) = fixture();
        control.install_inbound_build_authority(&a).await.unwrap();
        control.install_inbound_build_authority(&a).await.unwrap();
        assert_eq!(f.installs.load(Ordering::SeqCst), 1);
        let mut stale = a.clone();
        stale.operation_revision += 1;
        control
            .revoke_inbound_build_authority(&stale)
            .await
            .unwrap();
        assert_eq!(f.durable.lock().unwrap().as_ref(), Some(&a));
        control.revoke_inbound_build_authority(&a).await.unwrap();
        assert!(control.install_inbound_build_authority(&a).await.is_err());
        assert_eq!(f.installs.load(Ordering::SeqCst), 1);
        assert_eq!(f.revokes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn independently_mutated_authority_never_reaches_application() {
        let (control, f, a) = fixture();
        let encoded = serde_json::to_value(&a).unwrap();
        fn leaves(value: &serde_json::Value, path: Vec<String>, out: &mut Vec<Vec<String>>) {
            if let Some(object) = value.as_object() {
                for (k, v) in object {
                    let mut p = path.clone();
                    p.push(k.clone());
                    leaves(v, p, out);
                }
            } else {
                out.push(path);
            }
        }
        let mut paths = Vec::new();
        leaves(&encoded, vec![], &mut paths);
        for path in paths {
            let mut changed = encoded.clone();
            let mut leaf = &mut changed;
            for key in &path {
                leaf = &mut leaf[key];
            }
            *leaf = match leaf {
                serde_json::Value::String(s) => serde_json::Value::String(format!("{s}-changed")),
                serde_json::Value::Number(n) => serde_json::json!(n.as_i64().unwrap() + 1),
                _ => continue,
            };
            if let Ok(changed) = serde_json::from_value::<PublicInboundBuildAuthority>(changed) {
                assert!(
                    control
                        .install_inbound_build_authority(&changed)
                        .await
                        .is_err(),
                    "{path:?}"
                );
            }
        }
        assert_eq!(f.installs.load(Ordering::SeqCst), 0);
        assert!(f.durable.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn lost_waiter_retains_installation_and_retry_does_not_duplicate_work() {
        let (control, f, a) = fixture();
        f.hold.store(true, Ordering::SeqCst);
        let c = control.clone();
        let request = a.clone();
        let waiter = tokio::spawn(async move { c.install_inbound_build_authority(&request).await });
        f.entered.acquire().await.unwrap().forget();
        waiter.abort();
        let _ = waiter.await;
        f.release.add_permits(1);
        control.install_inbound_build_authority(&a).await.unwrap();
        assert_eq!(f.installs.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn partial_failure_and_failed_drain_never_publish_containment_or_successor() {
        let (control, f, a) = fixture();
        f.fail_install.store(true, Ordering::SeqCst);
        f.fail_drain.store(true, Ordering::SeqCst);
        assert!(control.install_inbound_build_authority(&a).await.is_err());
        assert!(f.durable.lock().unwrap().is_none());
        assert!(!*control.containment().borrow());
        assert!(control.install_inbound_build_authority(&a).await.is_err());
        assert!(control.shutdown_and_drain().await.is_err());
        assert!(!*control.containment().borrow());
        f.fail_drain.store(false, Ordering::SeqCst);
        control.shutdown_and_drain().await.unwrap();
        assert!(*control.containment().borrow());
    }

    #[tokio::test]
    async fn lost_install_owner_fences_and_retains_cleanup() {
        let (control, f, a) = fixture();
        f.panic_install.store(true, Ordering::SeqCst);
        assert!(control.install_inbound_build_authority(&a).await.is_err());
        let mut contained = control.containment();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            contained.wait_for(|done| *done),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(f.durable.lock().unwrap().is_none());
        assert!(!control.build_authority_open());
    }

    #[tokio::test]
    async fn lost_drain_owner_is_not_containment() {
        let (control, f, _) = fixture();
        f.panic_drain.store(true, Ordering::SeqCst);
        assert!(control.shutdown_and_drain().await.is_err());
        assert!(!*control.containment().borrow());
        assert!(!control.build_authority_open());
    }

    #[tokio::test]
    async fn shutdown_during_install_rejects_late_success() {
        let (control, f, a) = fixture();
        f.hold.store(true, Ordering::SeqCst);
        let c = control.clone();
        let request = a.clone();
        let install =
            tokio::spawn(async move { c.install_inbound_build_authority(&request).await });
        f.entered.acquire().await.unwrap().forget();
        let c = control.clone();
        let shutdown = tokio::spawn(async move { c.shutdown_and_drain().await });
        while !control.0.stopped.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
        f.release.add_permits(1);
        assert!(install.await.unwrap().is_err());
        shutdown.await.unwrap().unwrap();
        assert!(*control.containment().borrow());
        assert!(control.install_inbound_build_authority(&a).await.is_err());
    }
}
#[async_trait]
impl NativeRuntimeControl for CustomControl {
    fn custom(&self) -> bool {
        true
    }
    fn build_authority_open(&self) -> bool {
        !self.0.stopped.load(Ordering::Acquire)
    }
    fn containment(&self) -> watch::Receiver<bool> {
        self.0.contained.subscribe()
    }
    fn selected_epoch(&self) -> Option<crate::protocol::types::Epoch> {
        None
    }
    fn idle_replicas(&self) -> Result<Vec<ReplicaId>> {
        Err(RuntimeError::AuthorityNotAdmitted)
    }
    async fn configuration_idle_replicas(
        &self,
        configuration: &PublicConfiguration,
    ) -> Result<Vec<ReplicaId>> {
        if self.0.stopped.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let ids = self.0.application.idle_replicas(configuration).await?;
        if self.0.stopped.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        Ok(ids)
    }
    #[cfg(feature = "testing")]
    fn testing_tcp_binding(
        &self,
    ) -> Option<(
        ReplicaIdentity,
        ProcessSessionId,
        ReplicatorCreationReservation,
    )> {
        None
    }
    fn raw_retained_task_cardinality(&self) -> (usize, usize) {
        (0, 0)
    }

    async fn install_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        let owner = self.0.clone();
        let authority = authority.clone();
        retained(async move {
            let mut guard = ActionGuard(owner.clone(), false);
            let mut installed = owner.serial.lock().await;
            owner.scope(&authority).await?;
            if installed.as_ref() == Some(&authority) {
                return owner.context.validate_build_authority(&authority).await;
            }
            if installed.is_some() {
                return Err(RuntimeError::AuthorityMismatch(
                    "custom receiver successor requires containment or process replacement".into(),
                ));
            }
            owner
                .context
                .classify_replacement_inbound_build_authority(&authority)
                .await?;
            guard.1 = true;
            owner
                .context
                .install_replacement_inbound_build_authority(&authority)
                .await?;
            *installed = Some(authority.clone());
            let result = async {
                owner.context.validate_build_authority(&authority).await?;
                owner
                    .application
                    .install_inbound_build_authority(&authority)
                    .await?;
                owner.scope(&authority).await?;
                owner.context.validate_build_authority(&authority).await
            }
            .await;
            if let Err(error) = result {
                let cleanup = owner.fail_closed().await;
                guard.1 = false;
                return Err(RuntimeError::Application(format!(
                    "custom installation failed: {error}; containment: {cleanup:?}"
                )));
            }
            guard.1 = false;
            Ok(())
        })
        .await
    }

    async fn revoke_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        let owner = self.0.clone();
        let authority = authority.clone();
        retained(async move {
            let mut guard = ActionGuard(owner.clone(), false);
            let installed = owner.serial.lock().await;
            if installed.as_ref() != Some(&authority) {
                return Ok(());
            }
            if owner.revoked.load(Ordering::Acquire) {
                return Ok(());
            }
            guard.1 = true;
            // Ingress is invalidated before application cleanup can await.
            owner
                .context
                .revoke_replacement_inbound_build_authority()
                .await?;
            if let Err(error) = owner
                .application
                .revoke_inbound_build_authority(&authority)
                .await
            {
                let cleanup = owner.fail_closed().await;
                guard.1 = false;
                return Err(RuntimeError::Application(format!(
                    "custom revocation failed: {error}; containment: {cleanup:?}"
                )));
            }
            // Removing ingress does not prove the old native receiver joined.
            // Retain the receiver barrier for this process lifetime.
            owner.revoked.store(true, Ordering::Release);
            guard.1 = false;
            Ok(())
        })
        .await
    }

    async fn shutdown_and_drain(&self) -> Result<()> {
        // Stop admission synchronously, even when installation currently awaits.
        self.0.stopped.store(true, Ordering::Release);
        let owner = self.0.clone();
        retained(async move {
            let invalidation = owner
                .context
                .revoke_replacement_inbound_build_authority()
                .await;
            let _serial = owner.serial.lock().await;
            let drainage = owner.application.shutdown_and_drain().await;
            invalidation?;
            drainage?;
            owner.contained.send_replace(true);
            Ok(())
        })
        .await
    }
}
