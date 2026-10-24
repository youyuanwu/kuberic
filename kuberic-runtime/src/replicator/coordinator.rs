//! Creation-bound retained authority and drainage shared by every engine.

use super::*;
use crate::protocol::public_operations::{PublicConfiguration, PublicInboundBuildAuthority};
use tokio::sync::{Mutex, watch};

#[async_trait]
pub(super) trait Effects: Send + Sync {
    fn callback_ownership(&self) -> CallbackOwnership;
    fn observes_lifecycle(&self) -> bool {
        false
    }
    fn selected_epoch(&self) -> Option<crate::protocol::types::Epoch> {
        None
    }
    fn idle_replica_ids(&self) -> Result<Vec<ReplicaId>> {
        Err(RuntimeError::ProgressNotReady)
    }
    fn progress_query_readiness(&self) -> Result<bool> {
        Ok(true)
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
    fn successor_requires_replacement(&self) -> bool {
        false
    }
    async fn reject_successor(&self) -> Result<()> {
        Err(RuntimeError::AuthorityMismatch(
            "successor copy requires containment or process replacement".into(),
        ))
    }
    async fn install_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()>;
    async fn revoke_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()>;
    async fn idle_replicas(&self, configuration: &PublicConfiguration) -> Result<Vec<ReplicaId>>;
    async fn shutdown_and_drain(&self) -> Result<()>;
}

struct Owner {
    context: ReplicatorFactoryContext,
    effects: Arc<dyn Effects>,
    callback_ownership: CallbackOwnership,
    serial: Mutex<Option<PublicInboundBuildAuthority>>,
    stopped: AtomicBool,
    live: StdMutex<Option<PublicInboundBuildAuthority>>,
    revoked: AtomicBool,
    revocation_done: AtomicBool,
    drain: StdMutex<Option<std::result::Result<(), String>>>,
    contained: watch::Sender<bool>,
    compensations: StdMutex<Vec<Arc<Mutex<Compensation>>>>,
    #[cfg(test)]
    compensation_gate: StdMutex<Option<Arc<CompensationGate>>>,
}

struct Compensation {
    task: Option<tokio::task::JoinHandle<Result<()>>>,
    result: Option<std::result::Result<(), String>>,
}

#[cfg(test)]
struct CompensationGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

pub(super) struct Control(Arc<Owner>);

impl Control {
    pub(super) fn new(context: ReplicatorFactoryContext, effects: Arc<dyn Effects>) -> Self {
        let (contained, _) = watch::channel(false);
        Self(Arc::new(Owner {
            context,
            callback_ownership: effects.callback_ownership(),
            effects,
            serial: Mutex::new(None),
            stopped: AtomicBool::new(false),
            live: StdMutex::new(None),
            revoked: AtomicBool::new(false),
            revocation_done: AtomicBool::new(false),
            drain: StdMutex::new(None),
            contained,
            compensations: StdMutex::new(Vec::new()),
            #[cfg(test)]
            compensation_gate: StdMutex::new(None),
        }))
    }
}

impl Owner {
    fn stop(&self) {
        let mut live = self.live.lock().unwrap();
        self.stopped.store(true, Ordering::Release);
        *live = None;
    }

    fn activate(&self, authority: &PublicInboundBuildAuthority) -> Result<()> {
        let mut live = self.live.lock().unwrap();
        if self.stopped.load(Ordering::Acquire) || self.revoked.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        *live = Some(authority.clone());
        Ok(())
    }
    async fn scope(&self, authority: &PublicInboundBuildAuthority) -> Result<()> {
        if self.stopped.load(Ordering::Acquire)
            || authority.attempt.target != self.context.identity
            || authority.attempt.target_session != self.context.process_session_id
        {
            return Err(RuntimeError::AuthorityNotAdmitted);
        }
        self.context
            .access
            .validate_creation_build_authority(
                self.context
                    .reservation
                    .ok_or(RuntimeError::AuthorityNotAdmitted)?,
                authority,
                false,
            )
            .await
    }

    async fn drain_effects(&self) -> Result<()> {
        // Serial protects a single actual join; both errors and panics are sticky.
        let previous = self.drain.lock().unwrap().clone();
        if let Some(result) = previous {
            return result.map_err(RuntimeError::Application);
        }
        *self.drain.lock().unwrap() = Some(Err("native drain owner lost".into()));
        let result = self
            .effects
            .shutdown_and_drain()
            .await
            .map_err(|error| error.to_string());
        *self.drain.lock().unwrap() = Some(result.clone());
        result.map_err(RuntimeError::Application)
    }

    async fn fail_closed(&self, authority: &PublicInboundBuildAuthority) -> Result<()> {
        self.stop();
        let invalidation = self
            .context
            .revoke_exact_inbound_build_authority(authority)
            .await;
        let drainage = self.drain_effects().await;
        invalidation?;
        drainage?;
        self.contained.send_replace(true);
        Ok(())
    }
}

struct ActionGuard(Arc<Owner>, bool, PublicInboundBuildAuthority);
impl Drop for ActionGuard {
    fn drop(&mut self) {
        if self.1 {
            self.0.stop();
            let owner = self.0.clone();
            let authority = self.2.clone();
            #[cfg(test)]
            let gate = owner.compensation_gate.lock().unwrap().clone();
            let task = tokio::spawn(async move {
                #[cfg(test)]
                if let Some(gate) = gate {
                    gate.entered.notify_one();
                    gate.release.notified().await;
                }
                let _serial = owner.serial.lock().await;
                owner.fail_closed(&authority).await
            });
            self.0
                .compensations
                .lock()
                .unwrap()
                .push(Arc::new(Mutex::new(Compensation {
                    task: Some(task),
                    result: None,
                })));
        }
    }
}

async fn retained(
    action: impl std::future::Future<Output = Result<()>> + Send + 'static,
) -> Result<()> {
    // The task owns the action independently of the caller's response waiter.
    tokio::spawn(action).await.map_err(|error| {
        RuntimeError::Application(format!("controller authority owner lost: {error}"))
    })?
}

#[async_trait]
impl NativeRuntimeControl for Control {
    fn callback_ownership(&self) -> CallbackOwnership {
        self.0.callback_ownership
    }
    fn observes_lifecycle(&self) -> bool {
        self.0.effects.observes_lifecycle()
    }
    fn build_authority_open(&self) -> bool {
        !self.0.stopped.load(Ordering::Acquire)
    }
    fn inbound_build_live(&self, authority: &PublicInboundBuildAuthority) -> bool {
        self.0.live.lock().unwrap().as_ref() == Some(authority)
            && !self.0.stopped.load(Ordering::Acquire)
    }
    fn containment(&self) -> watch::Receiver<bool> {
        self.0.contained.subscribe()
    }
    fn selected_epoch(&self) -> Option<crate::protocol::types::Epoch> {
        self.0.effects.selected_epoch()
    }
    fn idle_replicas(&self) -> Result<Vec<ReplicaId>> {
        self.0.effects.idle_replica_ids()
    }
    async fn configuration_idle_replicas(
        &self,
        configuration: &PublicConfiguration,
    ) -> Result<Vec<ReplicaId>> {
        if self.0.stopped.load(Ordering::Acquire) {
            return Err(RuntimeError::Closed);
        }
        let ids = self.0.effects.idle_replicas(configuration).await?;
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
        self.0.effects.testing_tcp_binding()
    }
    fn raw_retained_task_cardinality(&self) -> (usize, usize) {
        self.0.effects.raw_retained_task_cardinality()
    }
    fn progress_query_readiness(&self) -> Result<bool> {
        self.0.effects.progress_query_readiness()
    }

    async fn install_inbound_build_authority(
        &self,
        authority: &PublicInboundBuildAuthority,
    ) -> Result<()> {
        let owner = self.0.clone();
        let authority = authority.clone();
        retained(async move {
            let mut installed = owner.serial.lock().await;
            let mut guard = ActionGuard(owner.clone(), false, authority.clone());
            owner.scope(&authority).await?;
            if installed.as_ref() == Some(&authority) {
                return owner.context.validate_build_authority(&authority).await;
            }
            owner
                .context
                .classify_replacement_inbound_build_authority(&authority)
                .await?;
            owner.scope(&authority).await?;
            if let Some(prior) = installed.as_ref() {
                if !owner.effects.successor_requires_replacement() {
                    let rejection = owner.effects.reject_successor().await;
                    return Err(RuntimeError::AuthorityMismatch(format!(
                        "successor receiver rejected: {rejection:?}"
                    )));
                }
                guard.2 = prior.clone();
                guard.1 = true;
                owner.stop();
                let rejection = owner.effects.reject_successor().await;
                let cleanup = owner.fail_closed(prior).await;
                guard.1 = false;
                return Err(RuntimeError::AuthorityMismatch(format!(
                    "successor receiver rejected: {rejection:?}; containment: {cleanup:?}"
                )));
            }
            guard.1 = true;
            owner
                .context
                .install_replacement_inbound_build_authority(&authority)
                .await?;
            owner.scope(&authority).await?;
            owner.activate(&authority)?;
            *installed = Some(authority.clone());
            let result = async {
                owner.context.validate_build_authority(&authority).await?;
                owner
                    .effects
                    .install_inbound_build_authority(&authority)
                    .await?;
                owner.scope(&authority).await?;
                owner.context.validate_build_authority(&authority).await
            }
            .await;
            if let Err(error) = result {
                let cleanup = owner.fail_closed(&authority).await;
                guard.1 = false;
                return Err(RuntimeError::Application(format!(
                    "installation failed: {error}; containment: {cleanup:?}"
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
        {
            let mut live = self.0.live.lock().unwrap();
            if live.as_ref() == Some(authority) {
                *live = None;
                self.0.revoked.store(true, Ordering::Release);
            }
        }
        let owner = self.0.clone();
        let authority = authority.clone();
        retained(async move {
            let installed = owner.serial.lock().await;
            let mut guard = ActionGuard(owner.clone(), false, authority.clone());
            if installed.as_ref() != Some(&authority) {
                return Ok(());
            }
            if owner.revocation_done.load(Ordering::Acquire) {
                return Ok(());
            }
            guard.1 = true;
            // Ingress is invalidated before application cleanup can await.
            *owner.live.lock().unwrap() = None;
            owner.revoked.store(true, Ordering::Release);
            owner
                .context
                .revoke_exact_inbound_build_authority(&authority)
                .await?;
            let drain_attempted = owner.drain.lock().unwrap().is_some();
            if drain_attempted {
                owner.drain_effects().await?;
                owner.revocation_done.store(true, Ordering::Release);
                guard.1 = false;
                return Ok(());
            }
            if let Err(error) = owner
                .effects
                .revoke_inbound_build_authority(&authority)
                .await
            {
                let cleanup = owner.fail_closed(&authority).await;
                guard.1 = false;
                return Err(RuntimeError::Application(format!(
                    "revocation failed: {error}; containment: {cleanup:?}"
                )));
            }
            // Removing ingress does not prove the old native receiver joined.
            // Retain the receiver barrier for this process lifetime.
            owner.revocation_done.store(true, Ordering::Release);
            guard.1 = false;
            Ok(())
        })
        .await
    }

    async fn shutdown_and_drain(&self) -> Result<()> {
        // Stop admission synchronously, even when installation currently awaits.
        self.0.stop();
        let owner = self.0.clone();
        retained(async move {
            let (invalidation, drainage) = {
                let serial = owner.serial.lock().await;
                let invalidation = match serial.as_ref() {
                    Some(authority) => {
                        owner
                            .context
                            .revoke_exact_inbound_build_authority(authority)
                            .await
                    }
                    None => Ok(()),
                };
                (invalidation, owner.drain_effects().await)
            };
            let tasks = owner.compensations.lock().unwrap().clone();
            let mut compensation = Ok(());
            for shared in tasks {
                let mut shared = shared.lock().await;
                if shared.result.is_none() {
                    let result = shared
                        .task
                        .as_mut()
                        .expect("unjoined compensation")
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result.map_err(|error| error.to_string()));
                    shared.task.take();
                    shared.result = Some(result);
                }
                if let Some(Err(error)) = shared.result.as_ref() {
                    compensation = Err(RuntimeError::Application(error.clone()));
                }
            }
            invalidation?;
            drainage?;
            compensation?;
            owner.contained.send_replace(true);
            Ok(())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{public_operations::*, types::*};
    use std::sync::atomic::AtomicUsize;
    use tokio::sync::Semaphore;

    // Contract/ownership fixture only. This is not a native engine witness.
    struct Fixture {
        expected: StdMutex<PublicInboundBuildAuthority>,
        durable: StdMutex<Option<PublicInboundBuildAuthority>>,
        generation: StdMutex<u64>,
        installs: AtomicUsize,
        drains: AtomicUsize,
        revokes: AtomicUsize,
        fail_drain: AtomicBool,
        fail_install: AtomicBool,
        panic_install: AtomicBool,
        panic_drain: AtomicBool,
        fail_revoke: AtomicBool,
        observer_callbacks: AtomicBool,
        drain_entered: tokio::sync::Semaphore,
        entered: Semaphore,
        release: Semaphore,
        hold: AtomicBool,
    }
    #[async_trait]
    impl PartitionAccessView for Fixture {
        async fn revoke_exact_inbound_build_authority(
            &self,
            authority: &PublicInboundBuildAuthority,
        ) -> Result<()> {
            if self.fail_revoke.load(Ordering::SeqCst) {
                return Err(RuntimeError::Application(
                    "retained exact cleanup failure".into(),
                ));
            }
            let mut current = self.durable.lock().unwrap();
            if current.as_ref() == Some(authority) {
                *current = None;
            }
            Ok(())
        }
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
        async fn validate_creation_build_authority(
            &self,
            _: ReplicatorCreationReservation,
            authority: &PublicInboundBuildAuthority,
            installed: bool,
        ) -> Result<()> {
            if authority != &*self.expected.lock().unwrap()
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
        async fn report_load(&self, _: Vec<LoadMetric>) -> Result<()> {
            Ok(())
        }
        async fn report_fault(&self, _: FaultType) -> Result<()> {
            Ok(())
        }
    }
    #[async_trait]
    impl CustomReplicatorControl for Fixture {
        fn callback_ownership(&self) -> CallbackOwnership {
            if self.observer_callbacks.load(Ordering::Acquire) {
                CallbackOwnership::NativeObserver
            } else {
                CallbackOwnership::ObjectOwnedOnInterruption
            }
        }
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
            self.drains.fetch_add(1, Ordering::SeqCst);
            self.drain_entered.add_permits(1);
            assert!(!self.panic_drain.load(Ordering::SeqCst), "lost drain owner");
            if self.fail_drain.load(Ordering::SeqCst) {
                Err(RuntimeError::Application("descendant remains".into()))
            } else {
                Ok(())
            }
        }
    }
    fn fixture() -> (Arc<Control>, Arc<Fixture>, PublicInboundBuildAuthority) {
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
            expected: StdMutex::new(authority.clone()),
            durable: StdMutex::new(None),
            generation: StdMutex::new(0),
            installs: AtomicUsize::new(0),
            drains: AtomicUsize::new(0),
            revokes: AtomicUsize::new(0),
            fail_drain: AtomicBool::new(false),
            fail_install: AtomicBool::new(false),
            panic_install: AtomicBool::new(false),
            panic_drain: AtomicBool::new(false),
            fail_revoke: AtomicBool::new(false),
            observer_callbacks: AtomicBool::new(false),
            drain_entered: tokio::sync::Semaphore::new(0),
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
            Arc::new(custom::control(context, fixture.clone())),
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
        control.revoke_inbound_build_authority(&a).await.unwrap();
        assert!(control.install_inbound_build_authority(&a).await.is_err());
        assert_eq!(f.installs.load(Ordering::SeqCst), 1);
        assert_eq!(f.revokes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn valid_successor_cannot_reuse_a_retained_receiver() {
        let (control, f, old) = fixture();
        control.install_inbound_build_authority(&old).await.unwrap();
        control.revoke_inbound_build_authority(&old).await.unwrap();
        let mut successor = old.clone();
        successor.operation_revision += 1;
        successor.operation_id = OperationId::new("successor");
        successor.attempt.build_id = OperationId::new("successor-build");
        *f.expected.lock().unwrap() = successor.clone();
        assert!(
            control
                .install_inbound_build_authority(&successor)
                .await
                .is_err()
        );
        assert_eq!(f.installs.load(Ordering::SeqCst), 1);
        assert_eq!(f.drains.load(Ordering::SeqCst), 0);
        assert!(f.durable.lock().unwrap().is_none());
        assert!(!control.inbound_build_live(&old));
        assert!(!control.inbound_build_live(&successor));
    }

    #[tokio::test]
    async fn companion_contract_is_creation_bound_and_does_not_require_diagnostics() {
        let (control, f, authority) = fixture();
        f.observer_callbacks.store(true, Ordering::Release);
        assert_eq!(
            control.callback_ownership(),
            CallbackOwnership::ObjectOwnedOnInterruption
        );
        assert!(!control.observes_lifecycle());
        assert!(control.idle_replicas().is_err());
        assert!(control.build_authority_open());
        control
            .install_inbound_build_authority(&authority)
            .await
            .unwrap();
        control.shutdown_and_drain().await.unwrap();
        assert!(*control.containment().borrow());
    }

    #[tokio::test]
    async fn shutdown_observers_share_compensation_join_and_sticky_failure() {
        let (control, f, authority) = fixture();
        let gate = Arc::new(CompensationGate {
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        *control.0.compensation_gate.lock().unwrap() = Some(gate.clone());
        f.panic_install.store(true, Ordering::SeqCst);
        assert!(
            control
                .install_inbound_build_authority(&authority)
                .await
                .is_err()
        );
        gate.entered.notified().await;
        let c = control.clone();
        let first = tokio::spawn(async move { c.shutdown_and_drain().await });
        f.drain_entered.acquire().await.unwrap().forget();
        first.abort();
        let _ = first.await;
        let c = control.clone();
        let mut second = tokio::spawn(async move { c.shutdown_and_drain().await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(100), &mut second)
                .await
                .is_err(),
            "a later observer skipped retained compensation"
        );
        f.fail_revoke.store(true, Ordering::SeqCst);
        gate.release.notify_one();
        assert!(second.await.unwrap().is_err());
        f.fail_revoke.store(false, Ordering::SeqCst);
        assert!(
            control.shutdown_and_drain().await.is_err(),
            "completed join failure was lost for later observers"
        );
        assert_eq!(f.drains.load(Ordering::SeqCst), 1);
        assert!(!*control.containment().borrow());
    }

    #[tokio::test]
    async fn late_cleanup_is_exact_and_successful_drain_is_not_repeated() {
        let (control, f, old) = fixture();
        control.install_inbound_build_authority(&old).await.unwrap();
        control.shutdown_and_drain().await.unwrap();
        let mut successor = old.clone();
        successor.operation_revision += 1;
        successor.attempt.target_session = ProcessSessionId::new("replacement-session");
        *f.durable.lock().unwrap() = Some(successor.clone());
        control.revoke_inbound_build_authority(&old).await.unwrap();
        control.shutdown_and_drain().await.unwrap();
        assert_eq!(f.durable.lock().unwrap().as_ref(), Some(&successor));
        assert_eq!(f.drains.load(Ordering::SeqCst), 1);
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
        assert!(control.shutdown_and_drain().await.is_err());
        assert_eq!(f.drains.load(Ordering::SeqCst), 1);
        assert!(!*control.containment().borrow());
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
        f.panic_drain.store(false, Ordering::SeqCst);
        assert!(control.shutdown_and_drain().await.is_err());
        assert_eq!(f.drains.load(Ordering::SeqCst), 1);
        assert!(!*control.containment().borrow());
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
