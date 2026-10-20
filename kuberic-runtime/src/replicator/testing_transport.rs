//! Socket destination routing for repository tests; native authority and protocol
//! inputs are untouched. Production without `testing` always dials directly.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use futures::future::BoxFuture;

use crate::protocol::types::{ProcessSessionId, ReplicatorCreationId};
use crate::replicator::ReplicatorCreation;

/// The actual built-in physical dial being redirected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestingTcpRouteKind {
    Replication,
    Build,
}

type Route = Arc<
    dyn Fn(TestingTcpRouteKind, String, String) -> BoxFuture<'static, std::io::Result<String>>
        + Send
        + Sync,
>;

struct Registration {
    route: Route,
    generation: Arc<()>,
    evidence: Option<(ProcessSessionId, Weak<ReplicatorCreation>)>,
}

fn routes() -> &'static Mutex<BTreeMap<String, Registration>> {
    static ROUTES: OnceLock<Mutex<BTreeMap<String, Registration>>> = OnceLock::new();
    ROUTES.get_or_init(Mutex::default)
}

pub(in crate::replicator) struct AttachmentRoute {
    instance: String,
    session: ProcessSessionId,
    generation: Weak<()>,
}

pub(in crate::replicator) fn before_attachment(
    instance: &str,
    session: &ProcessSessionId,
) -> Option<AttachmentRoute> {
    routes()
        .lock()
        .unwrap()
        .get_mut(instance)
        .map(|registration| {
            registration.evidence = None;
            registration.generation = Arc::new(());
            AttachmentRoute {
                instance: instance.to_owned(),
                session: session.clone(),
                generation: Arc::downgrade(&registration.generation),
            }
        })
}

pub(in crate::replicator) fn acknowledge(
    route: AttachmentRoute,
    creation: &Arc<ReplicatorCreation>,
) {
    let mut routes = routes().lock().unwrap();
    if let Some(registration) = routes.get_mut(&route.instance)
        && Weak::ptr_eq(&route.generation, &Arc::downgrade(&registration.generation))
    {
        registration.evidence = Some((route.session, Arc::downgrade(creation)));
    }
}

/// Owns the socket routing registration for one exact test replica instance.
pub struct TestingTcpRoute {
    instance: String,
}

impl TestingTcpRoute {
    /// Register before starting the host. Unregistered engines connect directly.
    pub fn register(
        instance: String,
        route: impl Fn(String, String) -> BoxFuture<'static, std::io::Result<String>>
        + Send
        + Sync
        + 'static,
    ) -> std::io::Result<Self> {
        Self::register_with_kind(instance, move |_, destination, endpoint| {
            route(destination, endpoint)
        })
    }

    /// Register a physical-dial callback, retaining the built-in dial kind.
    pub fn register_with_kind(
        instance: String,
        route: impl Fn(
            TestingTcpRouteKind,
            String,
            String,
        ) -> BoxFuture<'static, std::io::Result<String>>
        + Send
        + Sync
        + 'static,
    ) -> std::io::Result<Self> {
        let mut routes = routes().lock().unwrap();
        if routes.contains_key(&instance) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "instance already routed",
            ));
        }
        routes.insert(
            instance.clone(),
            Registration {
                route: Arc::new(route),
                generation: Arc::new(()),
                evidence: None,
            },
        );
        Ok(Self { instance })
    }

    /// Whether a successfully attached built-in creation uses an owned route.
    ///
    /// Installing a callback alone does not establish this capability.
    pub fn supports(session: &ProcessSessionId, creation: &ReplicatorCreationId) -> bool {
        routes().lock().unwrap().values().any(|registration| {
            registration
                .evidence
                .as_ref()
                .is_some_and(|(bound_session, owner)| {
                    bound_session == session
                        && owner.upgrade().is_some_and(|owner| {
                            owner
                                .identity
                                .get()
                                .is_some_and(|identity| identity.to_public_id() == *creation)
                                && !owner.armed.load(std::sync::atomic::Ordering::Acquire)
                        })
                })
        })
    }
}

impl Drop for TestingTcpRoute {
    fn drop(&mut self) {
        routes().lock().unwrap().remove(&self.instance);
    }
}

pub(super) async fn route(
    kind: TestingTcpRouteKind,
    instance: &str,
    destination: &str,
    endpoint: String,
) -> std::io::Result<String> {
    let route = routes()
        .lock()
        .unwrap()
        .get(instance)
        .map(|registration| registration.route.clone());
    match route {
        Some(route) => route(kind, destination.to_owned(), endpoint).await,
        None => Ok(endpoint),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capabilities::{ReplicatorCreationIdentity, RuntimeHostToken};
    use crate::protocol::types::{Epoch, ReplicaRole};
    use crate::replicator::{Replicator, ReplicatorCapabilities};
    use async_trait::async_trait;

    struct Control;

    #[async_trait]
    impl Replicator for Control {
        async fn open(&self) -> crate::Result<String> {
            Ok(String::new())
        }
        async fn close(&self) -> crate::Result<()> {
            Ok(())
        }
        async fn change_role(&self, _: Epoch, _: ReplicaRole) -> crate::Result<()> {
            Ok(())
        }
        async fn update_epoch(&self, _: Epoch) -> crate::Result<()> {
            Ok(())
        }
        fn abort(&self) {}
        async fn current_progress(&self) -> crate::Result<i64> {
            Ok(0)
        }
        async fn catch_up_capability(&self) -> crate::Result<i64> {
            Ok(0)
        }
    }

    fn creation() -> Arc<ReplicatorCreation> {
        Arc::new(ReplicatorCreation::new(
            Arc::new(Control),
            Some(ReplicatorCreationIdentity::new(RuntimeHostToken::new())),
            ReplicatorCapabilities::conservative(),
        ))
    }

    fn instance() -> String {
        uuid::Uuid::new_v4().to_string()
    }

    #[tokio::test]
    async fn dial_kinds_and_legacy_callbacks_leave_canonical_inputs_intact() {
        let instance = instance();
        let canonical = "http://127.0.0.1:12345".to_owned();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let received = calls.clone();
        let owner = TestingTcpRoute::register_with_kind(
            instance.clone(),
            move |kind, destination, endpoint| {
                received.lock().unwrap().push((kind, destination, endpoint));
                Box::pin(async { Ok("http://127.0.0.1:54321".to_owned()) })
            },
        )
        .unwrap();
        for kind in [TestingTcpRouteKind::Replication, TestingTcpRouteKind::Build] {
            assert_eq!(
                route(kind, &instance, "peer", canonical.clone())
                    .await
                    .unwrap(),
                "http://127.0.0.1:54321"
            );
        }
        assert_eq!(
            *calls.lock().unwrap(),
            [
                (
                    TestingTcpRouteKind::Replication,
                    "peer".into(),
                    canonical.clone()
                ),
                (TestingTcpRouteKind::Build, "peer".into(), canonical.clone()),
            ]
        );
        drop(owner);
        assert_eq!(
            route(
                TestingTcpRouteKind::Build,
                &instance,
                "peer",
                canonical.clone()
            )
            .await
            .unwrap(),
            canonical
        );
        let _owner = TestingTcpRoute::register(instance.clone(), |destination, endpoint| {
            Box::pin(async move {
                assert_eq!(destination, "peer");
                Ok(endpoint)
            })
        })
        .unwrap();
        assert_eq!(
            route(
                TestingTcpRouteKind::Build,
                &instance,
                "peer",
                canonical.clone()
            )
            .await
            .unwrap(),
            canonical
        );
    }

    #[test]
    fn callback_installation_cannot_attest_and_uncommitted_creation_is_not_supported() {
        let instance = instance();
        let owner = TestingTcpRoute::register(instance.clone(), |_, endpoint| {
            Box::pin(async { Ok(endpoint) })
        })
        .unwrap();
        let session = ProcessSessionId::new(instance.clone());
        let creation = creation();
        let id = creation.identity.get().unwrap().to_public_id();
        assert!(!TestingTcpRoute::supports(&session, &id));
        let candidate = before_attachment(&instance, &session).unwrap();
        acknowledge(candidate, &creation);
        assert!(!TestingTcpRoute::supports(&session, &id));
        creation
            .armed
            .store(false, std::sync::atomic::Ordering::Release);
        assert!(TestingTcpRoute::supports(&session, &id));
        assert!(!TestingTcpRoute::supports(
            &ProcessSessionId::new("other"),
            &id
        ));
        assert!(!TestingTcpRoute::supports(
            &session,
            &ReplicatorCreationId::new("other")
        ));
        drop(owner);
        assert!(!TestingTcpRoute::supports(&session, &id));
    }

    #[test]
    fn failed_replacement_or_dropped_creation_revokes_evidence() {
        let instance = instance();
        let _route = TestingTcpRoute::register(instance.clone(), |_, endpoint| {
            Box::pin(async { Ok(endpoint) })
        })
        .unwrap();
        let session = ProcessSessionId::new(instance.clone());
        let first = creation();
        first
            .armed
            .store(false, std::sync::atomic::Ordering::Release);
        let first_id = first.identity.get().unwrap().to_public_id();
        acknowledge(before_attachment(&instance, &session).unwrap(), &first);
        assert!(TestingTcpRoute::supports(&session, &first_id));
        let _failed_attempt = before_attachment(&instance, &session).unwrap();
        assert!(!TestingTcpRoute::supports(&session, &first_id));
        let next = creation();
        next.armed
            .store(false, std::sync::atomic::Ordering::Release);
        let next_id = next.identity.get().unwrap().to_public_id();
        let next_session = ProcessSessionId::new("new-session");
        acknowledge(before_attachment(&instance, &next_session).unwrap(), &next);
        assert!(!TestingTcpRoute::supports(&session, &first_id));
        assert!(TestingTcpRoute::supports(&next_session, &next_id));
        drop(next);
        assert!(!TestingTcpRoute::supports(&next_session, &next_id));
    }

    #[test]
    fn retired_route_cannot_acknowledge_new_registration() {
        let instance = instance();
        let owner = TestingTcpRoute::register(instance.clone(), |_, endpoint| {
            Box::pin(async { Ok(endpoint) })
        })
        .unwrap();
        let session = ProcessSessionId::new(instance.clone());
        let candidate = before_attachment(&instance, &session).unwrap();
        let creation = creation();
        creation
            .armed
            .store(false, std::sync::atomic::Ordering::Release);
        let id = creation.identity.get().unwrap().to_public_id();
        drop(owner);
        let _next =
            TestingTcpRoute::register(instance, |_, endpoint| Box::pin(async { Ok(endpoint) }))
                .unwrap();
        acknowledge(candidate, &creation);
        assert!(!TestingTcpRoute::supports(&session, &id));
    }

    #[test]
    fn superseded_attachment_cannot_overwrite_new_host_evidence() {
        let instance = instance();
        let _owner = TestingTcpRoute::register(instance.clone(), |_, endpoint| {
            Box::pin(async { Ok(endpoint) })
        })
        .unwrap();
        let old_session = ProcessSessionId::new("old-session");
        let old = creation();
        old.armed.store(false, std::sync::atomic::Ordering::Release);
        let stale = before_attachment(&instance, &old_session).unwrap();
        let new_session = ProcessSessionId::new("new-session");
        let new = creation();
        new.armed.store(false, std::sync::atomic::Ordering::Release);
        let current = before_attachment(&instance, &new_session).unwrap();
        acknowledge(current, &new);
        acknowledge(stale, &old);
        assert!(!TestingTcpRoute::supports(
            &old_session,
            &old.identity.get().unwrap().to_public_id()
        ));
        assert!(TestingTcpRoute::supports(
            &new_session,
            &new.identity.get().unwrap().to_public_id()
        ));
    }

    mod attachment {
        use super::*;
        use crate::application::{OperationDataStream, StateProvider};
        use crate::host::store::PublicInboundBuildAdmission;
        use crate::protocol::public_operations::{
            PublicInboundBuildAuthority, PublicV1Identity, PublicV1LifecycleBinding,
            StatePersistence,
        };
        use crate::protocol::types::{
            AccessStatus, AgentGeneration, FaultType, LoadMetric, PartitionId,
            PartitionInformation, ReplicaId, ReplicaIdentity, ReplicaInstanceId,
            ReplicaStorageIncarnationContext, ResourceUid,
        };
        use crate::replicator::{
            DefaultReplicatorFactory, PartitionAccessView, ReplicatorAttachment,
            ReplicatorCreationReservation, ReplicatorFactory, ReplicatorFactoryContext,
            ReplicatorInterfaces, ReplicatorRegistration, ReplicatorSettings,
            StatefulServicePartition,
        };
        use crate::{Result, RuntimeError};

        #[derive(Default)]
        struct Fixture {
            fail_registration: std::sync::atomic::AtomicBool,
            binding: Mutex<Option<(ReplicaIdentity, ProcessSessionId)>>,
        }

        #[async_trait]
        impl ReplicatorRegistration for Fixture {
            fn reserve_replicator_creation(&self) -> Result<ReplicatorCreationReservation> {
                Ok(ReplicatorCreationReservation::new(RuntimeHostToken::new()))
            }
            fn cancel_replicator_creation(&self, _: ReplicatorCreationReservation) {}
            async fn register_interfaces(
                &self,
                _: &ReplicatorAttachment,
                _: Option<Arc<dyn StateProvider>>,
                _: ReplicatorCreationReservation,
            ) -> Result<()> {
                if self
                    .fail_registration
                    .load(std::sync::atomic::Ordering::Acquire)
                {
                    Err(RuntimeError::Closed)
                } else {
                    Ok(())
                }
            }
        }

        #[async_trait]
        impl PartitionAccessView for Fixture {
            fn validate_native_creation(&self, _: ReplicatorCreationReservation) -> Result<()> {
                Ok(())
            }
            async fn native_credentials(&self) -> Result<Arc<str>> {
                Ok(Arc::from("route-fixture"))
            }

            async fn native_local_endpoint(
                &self,
                address: String,
            ) -> Result<(
                crate::protocol::types::HistoryContext,
                crate::host::native_transport::NativeEndpoint,
            )> {
                let (identity, process_session_id) = self.binding.lock().unwrap().clone().unwrap();
                let resource = ResourceUid::new(identity.instance_id.as_str());
                let generation = crate::protocol::types::PublicProtocolGeneration::new(51);
                let initialization =
                    crate::protocol::types::derive_history_initialization_id(&resource, generation);
                let history = crate::protocol::types::derive_bootstrap_history(
                    &resource,
                    generation,
                    &initialization,
                );
                let endpoint = crate::host::native_transport::NativeEndpoint {
                    storage: ReplicaStorageIncarnationContext {
                        replica: identity.clone(),
                        storage: crate::protocol::types::ReplicaStorageIdentity::Persistent {
                            pod_uid: crate::protocol::types::PodUid::new(
                                identity.instance_id.as_str(),
                            ),
                            pvc_uid: crate::protocol::types::PvcUid::new(
                                identity.instance_id.as_str(),
                            ),
                        },
                        storage_generation: crate::protocol::types::StorageGeneration::new(
                            identity.instance_id.as_str(),
                        ),
                    },
                    identity,
                    process_session_id,
                    address,
                };
                Ok((history, endpoint))
            }

            fn partition_information(&self) -> PartitionInformation {
                PartitionInformation {
                    partition_id: PartitionId::new("routing-evidence"),
                }
            }
            async fn read_status(&self) -> Result<AccessStatus> {
                unreachable!()
            }
            async fn write_status(&self) -> Result<AccessStatus> {
                unreachable!()
            }
            async fn classify_replacement_inbound_build_authority(
                &self,
                _: &PublicInboundBuildAuthority,
            ) -> Result<PublicInboundBuildAdmission> {
                unreachable!()
            }
            async fn install_replacement_inbound_build_authority(
                &self,
                _: &PublicInboundBuildAuthority,
            ) -> Result<()> {
                unreachable!()
            }
            async fn revoke_replacement_inbound_build_authority(&self) -> Result<()> {
                unreachable!()
            }
            fn release_provider_writer(&self) {}
            async fn report_load(&self, _: Vec<LoadMetric>) -> Result<()> {
                unreachable!()
            }
            async fn report_fault(&self, _: FaultType) -> Result<()> {
                unreachable!()
            }
        }

        #[async_trait]
        impl StateProvider for Fixture {
            async fn update_epoch(&self, _: Epoch, _: i64) -> Result<()> {
                unreachable!()
            }
            async fn last_committed_lsn(&self) -> Result<i64> {
                unreachable!()
            }
            async fn get_copy_context(&self) -> Result<OperationDataStream> {
                unreachable!()
            }
            async fn get_copy_state(
                &self,
                _: i64,
                _: OperationDataStream,
            ) -> Result<OperationDataStream> {
                unreachable!()
            }
            async fn on_data_loss(&self) -> Result<bool> {
                unreachable!()
            }
        }

        struct CustomFactory;

        struct ReplayedFactory {
            context: Mutex<Option<ReplicatorFactoryContext>>,
        }

        #[async_trait]
        impl ReplicatorFactory for ReplayedFactory {
            async fn create_replicator(
                &self,
                context: ReplicatorFactoryContext,
                provider: Option<Arc<dyn StateProvider>>,
                settings: ReplicatorSettings,
            ) -> Result<ReplicatorInterfaces> {
                let context = self.context.lock().unwrap().get_or_insert(context).clone();
                DefaultReplicatorFactory::new()
                    .create_replicator(context, provider, settings)
                    .await
            }
        }

        #[async_trait]
        impl ReplicatorFactory for CustomFactory {
            async fn create_replicator(
                &self,
                _: ReplicatorFactoryContext,
                _: Option<Arc<dyn StateProvider>>,
                _: ReplicatorSettings,
            ) -> Result<ReplicatorInterfaces> {
                Ok(ReplicatorInterfaces::secondary(
                    Arc::new(Control),
                    None,
                    ReplicatorCapabilities::conservative(),
                ))
            }
        }

        fn partition(
            instance: &str,
            session: &ProcessSessionId,
            fixture: Arc<Fixture>,
        ) -> StatefulServicePartition {
            *fixture.binding.lock().unwrap() = Some((
                ReplicaIdentity {
                    replica_id: ReplicaId::new(1),
                    instance_id: ReplicaInstanceId::new(instance),
                    agent_generation: AgentGeneration::new(instance),
                },
                session.clone(),
            ));
            StatefulServicePartition::new(
                RuntimeHostToken::new(),
                fixture.clone(),
                ReplicatorFactoryContext::new(
                    RuntimeHostToken::new(),
                    ReplicaIdentity {
                        replica_id: ReplicaId::new(1),
                        instance_id: ReplicaInstanceId::new(instance),
                        agent_generation: AgentGeneration::new(instance),
                    },
                    session.clone(),
                    fixture,
                    PublicV1LifecycleBinding {
                        identity: PublicV1Identity::new(51),
                        resource_uid: ResourceUid::new(instance),
                        spec_generation: 1,
                        state_persistence: StatePersistence::Persisted,
                    },
                    "127.0.0.1:0".parse().unwrap(),
                ),
            )
        }

        #[tokio::test]
        async fn actual_builtin_attachment_not_callback_or_custom_engine_establishes_support() {
            let instance = instance();
            let session = ProcessSessionId::new(instance.clone());
            let _route = TestingTcpRoute::register(instance.clone(), |_, endpoint| {
                Box::pin(async { Ok(endpoint) })
            })
            .unwrap();
            let fixture = Arc::new(Fixture {
                fail_registration: false.into(),
                ..Default::default()
            });
            let partition = partition(&instance, &session, fixture.clone());
            let settings = Some(ReplicatorSettings {
                replication_address: "http://127.0.0.1:0".into(),
            });
            let custom = partition
                .with_factory(Arc::new(CustomFactory))
                .create_replicator(None, settings.clone())
                .await
                .unwrap();
            let custom_id = custom.creation.identity.get().unwrap().to_public_id();
            assert!(!TestingTcpRoute::supports(&session, &custom_id));
            let partition = partition.with_factory(Arc::new(DefaultReplicatorFactory::new()));
            let built_in = partition
                .create_replicator(Some(fixture.clone()), settings.clone())
                .await
                .unwrap();
            let built_in_id = built_in.creation.identity.get().unwrap().to_public_id();
            assert!(TestingTcpRoute::supports(&session, &built_in_id));
            fixture
                .fail_registration
                .store(true, std::sync::atomic::Ordering::Release);
            assert!(
                partition
                    .create_replicator(Some(fixture), settings)
                    .await
                    .is_err()
            );
            assert!(!TestingTcpRoute::supports(&session, &built_in_id));
        }

        #[tokio::test]
        async fn construction_without_owned_route_cannot_later_self_attest() {
            let instance = instance();
            let session = ProcessSessionId::new(instance.clone());
            let fixture = Arc::new(Fixture {
                fail_registration: false.into(),
                ..Default::default()
            });
            let partition = partition(&instance, &session, fixture.clone())
                .with_factory(Arc::new(DefaultReplicatorFactory::new()));
            let built_in = partition
                .create_replicator(
                    Some(fixture),
                    Some(ReplicatorSettings {
                        replication_address: "http://127.0.0.1:0".into(),
                    }),
                )
                .await
                .unwrap();
            let id = built_in.creation.identity.get().unwrap().to_public_id();
            let _route =
                TestingTcpRoute::register(instance, |_, endpoint| Box::pin(async { Ok(endpoint) }))
                    .unwrap();
            assert!(!TestingTcpRoute::supports(&session, &id));
        }

        #[tokio::test]
        async fn replayed_actual_engine_context_cannot_attest_new_host_or_creation() {
            let instance = instance();
            let session = ProcessSessionId::new(instance.clone());
            let _route = TestingTcpRoute::register(instance.clone(), |_, endpoint| {
                Box::pin(async { Ok(endpoint) })
            })
            .unwrap();
            let fixture = Arc::new(Fixture {
                fail_registration: false.into(),
                ..Default::default()
            });
            let factory = Arc::new(ReplayedFactory {
                context: Mutex::new(None),
            });
            let settings = Some(ReplicatorSettings {
                replication_address: "http://127.0.0.1:0".into(),
            });
            let first = partition(&instance, &session, fixture.clone())
                .with_factory(factory.clone())
                .create_replicator(Some(fixture.clone()), settings.clone())
                .await
                .unwrap();
            let first_id = first.creation.identity.get().unwrap().to_public_id();
            assert!(TestingTcpRoute::supports(&session, &first_id));
            for changed_session in [
                session.clone(),
                ProcessSessionId::new("changed-host-session"),
            ] {
                let replayed = partition(&instance, &changed_session, fixture.clone())
                    .with_factory(factory.clone())
                    .create_replicator(Some(fixture.clone()), settings.clone())
                    .await;
                assert!(matches!(replayed, Err(RuntimeError::AuthorityNotAdmitted)));
                assert!(!TestingTcpRoute::supports(&session, &first_id));
            }
        }
    }
}
