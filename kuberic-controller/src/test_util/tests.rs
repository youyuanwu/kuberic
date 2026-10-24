use super::cluster::{ConfigurationEvidence, Harness};
use super::*;
use anyhow::{Context, Result, ensure};
use kuberic_runtime::{
    StateProvider, StatefulServiceReplica,
    application::{OpenContext, OperationDataStream, RoleChange},
    protocol::{public_operations::*, types::*},
    replicator::{DefaultReplicatorFactory, Replicator, ReplicatorSettings, StateReplicator},
};
use std::sync::Arc;

pub(super) struct EmptyApplication {
    stop: tokio::sync::watch::Sender<bool>,
    streams: tokio::sync::Mutex<tokio::task::JoinSet<kuberic_runtime::Result<()>>>,
    control: tokio::sync::Mutex<Option<Arc<dyn Replicator>>>,
    state: tokio::sync::Mutex<Option<Arc<dyn StateReplicator>>>,
    role: std::sync::Mutex<ReplicaRole>,
}
impl Default for EmptyApplication {
    fn default() -> Self {
        Self {
            stop: tokio::sync::watch::channel(false).0,
            streams: Default::default(),
            control: Default::default(),
            state: Default::default(),
            role: std::sync::Mutex::new(ReplicaRole::None),
        }
    }
}
impl EmptyApplication {
    async fn drain(&self) -> super::Result<()> {
        self.stop.send_replace(true);
        let mut tasks = self.streams.lock().await;
        while let Some(result) = tasks.join_next().await {
            result
                .map_err(anyhow::Error::from)?
                .map_err(anyhow::Error::from)?;
        }
        Ok(())
    }
}
struct EmptyState;
pub(super) struct EmptyFactory;

#[async_trait::async_trait]
impl StateProvider for EmptyState {
    async fn update_epoch(&self, _: Epoch, _: i64) -> kuberic_runtime::Result<()> {
        Ok(())
    }
    async fn last_committed_lsn(&self) -> kuberic_runtime::Result<i64> {
        Ok(0)
    }
    async fn get_copy_context(&self) -> kuberic_runtime::Result<OperationDataStream> {
        Ok(Box::pin(futures::stream::empty()))
    }
    async fn get_copy_state(
        &self,
        _: i64,
        _: OperationDataStream,
    ) -> kuberic_runtime::Result<OperationDataStream> {
        Ok(Box::pin(futures::stream::iter([Ok(
            kuberic_runtime::application::OperationData::from_static(b"empty-provider-v1"),
        )])))
    }
    async fn on_data_loss(&self) -> kuberic_runtime::Result<bool> {
        Ok(false)
    }
}
#[async_trait::async_trait]
impl StatefulServiceReplica for EmptyApplication {
    async fn open(
        self: Arc<Self>,
        context: OpenContext,
    ) -> kuberic_runtime::Result<Arc<dyn Replicator>> {
        let mut control = self.control.lock().await;
        if let Some(control) = control.as_ref() {
            return Ok(control.clone());
        }
        let interfaces = context
            .partition
            .with_factory(Arc::new(DefaultReplicatorFactory::new()))
            .create_replicator(
                Some(Arc::new(EmptyState)),
                Some(ReplicatorSettings {
                    replication_address: "http://127.0.0.1:0".into(),
                }),
            )
            .await?;
        *self.state.lock().await = interfaces.state_replicator();
        let replicator = interfaces.replicator();
        *control = Some(replicator.clone());
        Ok(replicator)
    }
    async fn change_role(&self, role: ReplicaRole) -> kuberic_runtime::Result<RoleChange> {
        let previous = *self.role.lock().unwrap();
        if matches!(
            role,
            ReplicaRole::IdleSecondary | ReplicaRole::ActiveSecondary
        ) && !matches!(
            previous,
            ReplicaRole::IdleSecondary | ReplicaRole::ActiveSecondary
        ) {
            let state = self
                .state
                .lock()
                .await
                .clone()
                .ok_or(kuberic_runtime::RuntimeError::NotOpen)?;
            let mut streams = vec![state.replication_operations()?];
            if previous != ReplicaRole::Primary {
                streams.push(state.copy_operations()?);
            }
            let mut tasks = self.streams.lock().await;
            for mut stream in streams {
                let mut stop = self.stop.subscribe();
                tasks.spawn(async move {
                    tokio::select! {
                        biased;
                        _ = stop.wait_for(|stopped| *stopped) => Ok(()),
                        result = async {
                            loop {
                                let operation = match stream.get_operation().await {
                                    Ok(Some(operation)) => operation,
                                    Ok(None) | Err(kuberic_runtime::RuntimeError::Closed) => break,
                                    Err(error) => return Err(error),
                                };
                                if !operation.end_of_stream
                                    && (operation.kind != kuberic_runtime::replicator::stream::OperationKind::Copy
                                        || operation.data.as_ref() != b"empty-provider-v1")
                                {
                                    let reason = "readiness-only application received mutable state".to_owned();
                                    stream.report_fault(reason.clone())?;
                                    return Err(kuberic_runtime::RuntimeError::Application(reason));
                                }
                                operation.acknowledge()?;
                            }
                            Ok(())
                        } => result,
                    }
                });
            }
        }
        *self.role.lock().unwrap() = role;
        Ok(RoleChange {
            service_address: None,
        })
    }
    async fn close(&self) -> kuberic_runtime::Result<()> {
        self.stop.send_replace(true);
        Ok(())
    }
    fn abort(&self) {
        self.stop.send_replace(true);
    }
}
#[async_trait::async_trait]
impl ApplicationFactory for EmptyFactory {
    type Service = EmptyApplication;
    async fn create(
        &self,
        context: ReplicaContext,
    ) -> super::Result<CreatedApplication<EmptyApplication>> {
        assert!(
            !context
                .environment()
                .contains_key("KUBERIC_AGENT_BEARER_TOKEN")
        );
        assert_eq!(
            context
                .environment()
                .get("KUBERIC_POD_UID")
                .map(String::as_str),
            Some(context.identity().instance_id.as_str())
        );
        assert_eq!(
            context
                .environment()
                .get("KUBERIC_DATA_ROOT")
                .map(String::as_str),
            context
                .application_root()
                .parent()
                .and_then(std::path::Path::to_str)
        );
        let marker = context.application_root().join("empty-provider");
        let fresh = std::fs::read_dir(context.application_root())?
            .next()
            .is_none();
        if fresh {
            std::fs::write(&marker, b"empty-provider-v1")?;
        } else if std::fs::read(marker)? != b"empty-provider-v1" {
            return Err(TestError::new(
                ErrorKind::Application,
                "invalid minimal provider format",
            ));
        }
        Ok(CreatedApplication::new(
            Arc::new(EmptyApplication::default()),
            if fresh {
                kuberic_runtime::host::ApplicationStorageState::FreshEmpty
            } else {
                kuberic_runtime::host::ApplicationStorageState::Established
            },
        )
        .with_storage_paths([context.application_root().to_owned()]))
    }
    async fn shutdown_and_drain(&self, service: Arc<EmptyApplication>) -> super::Result<()> {
        service.drain().await
    }
}

fn rewrite_detached_configuration(
    status: &crate::crd::KubericSetStatus,
    evidence: &ConfigurationEvidence,
    change: impl Fn(&mut PublicOperationProgram),
) -> Result<(crate::crd::KubericSetStatus, ConfigurationEvidence)> {
    let mut status = status.clone();
    let mut evidence = evidence.clone();
    let mut authority = status
        .public_v1_authority
        .as_ref()
        .unwrap()
        .authority()
        .map_err(anyhow::Error::msg)?;
    for (intent, completion) in evidence.operations.values_mut() {
        let old = PublicV1AcceptedOperationCompletion::from_completion(completion);
        let accepted = authority
            .completed_operations
            .iter_mut()
            .find(|accepted| **accepted == old)
            .context("detached baseline not accepted")?;
        let program = intent.program.as_mut().unwrap();
        change(program);
        intent.input_digest = program.digest();
        completion.input_digest = intent.input_digest.clone();
        if matches!(program, PublicOperationProgram::Epoch { .. }) {
            completion.call_kind = PublicV1CallKind::Epoch;
            completion.current_configuration_id = None;
        }
        intent.validate().map_err(anyhow::Error::msg)?;
        *accepted = PublicV1AcceptedOperationCompletion::from_completion(completion);
    }
    status.public_v1_authority = Some(
        crate::crd::PublicV1AuthorityStatus::from_authority(&authority)
            .map_err(anyhow::Error::msg)?,
    );
    Ok((status, evidence))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn readiness_rejects_detached_stale_sessions_configuration_and_writers() -> Result<()> {
    type Core = Harness<EmptyFactory>;
    let mut harness = Harness::new(
        EmptyFactory,
        "in-process".into(),
        "sample".into(),
        "in-process/empty".into(),
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(20),
        3,
    )
    .await?;
    let result: Result<()> = async {
        if harness.ordinary_integration_unavailable().await? {
            return Ok(());
        }
        let mut barrier = harness.api.arm_operation_barrier("sample")?;
        let pending = loop {
            tokio::select! {pending=barrier.entered()=>break pending?,step=harness.step()=>step?,}
        };
        barrier.release();
        harness.ready().await?;
        let status = harness.set().await?.status.context("status missing")?;
        let mut reports = Vec::new();
        for id in 1..=3 {
            reports.push(harness.report(id).await?);
        }
        ensure!(Core::settled(&status, &reports, 3));
        let evidence = harness.configuration_evidence(&reports).await?;
        ensure!(Core::configurations_settled(&status, &reports, &evidence));
        let mut pending_transfer = status.clone();
        let topology = pending_transfer.public_v1_topology.as_mut().unwrap();
        topology.pending_switchover = Some(crate::crd::PlannedSwitchoverRequestSpec {
            request_id: "pending-transfer".into(),
            target_replica_id: topology
                .sessions
                .iter()
                .find(|session| session.identity.replica_id != topology.primary_id)
                .unwrap()
                .identity
                .replica_id
                .value() as u32,
        });
        ensure!(!Core::settled(&pending_transfer, &reports, 3));
        ensure!(!Core::configurations_settled(
            &pending_transfer,
            &reports,
            &evidence
        ));
        let (altered_status, altered_evidence) =
            rewrite_detached_configuration(&status, &evidence, |program| {
                if let PublicOperationProgram::Configuration(public)
                | PublicOperationProgram::CatchUp {
                    configuration: public,
                    ..
                } = program
                    && let Some(peer) = public.replicas.first_mut()
                {
                    peer.process_session_id = ProcessSessionId::new("stale-peer");
                }
            })?;
        ensure!(!Core::configurations_settled(
            &altered_status,
            &reports,
            &altered_evidence
        ));
        let (unrelated_status, unrelated) =
            rewrite_detached_configuration(&status, &evidence, |program| {
                *program = PublicOperationProgram::Epoch {
                    epoch: status.public_v1_topology.as_ref().unwrap().epoch,
                };
            })?;
        ensure!(!Core::configurations_settled(
            &unrelated_status,
            &reports,
            &unrelated
        ));
        let (historical_status, historical) =
            rewrite_detached_configuration(&status, &evidence, |program| {
                if let PublicOperationProgram::Configuration(public)
                | PublicOperationProgram::CatchUp {
                    configuration: public,
                    ..
                } = program
                {
                    let mut epoch = public.current.epoch;
                    epoch.configuration_number += 1;
                    public.current = ConfigurationDescriptor::new(
                        epoch,
                        public.current.primary_id,
                        public.current.members.clone(),
                        public.current.write_quorum,
                    );
                }
            })?;
        ensure!(!Core::configurations_settled(
            &historical_status,
            &reports,
            &historical
        ));
        for field in ["publicV1Operation", "publicV1Transition"] {
            let mut altered = serde_json::to_value(&status)?;
            altered[field] = pending["status"][field].clone();
            ensure!(
                !altered[field].is_null(),
                "pending observation missing {field}"
            );
            ensure!(!Core::settled(
                &serde_json::from_value(altered)?,
                &reports,
                3
            ));
        }
        // Pending replacement and renewal must independently prevent readiness.
        let mut replacement = status.clone();
        replacement.public_v1_replacement = Some(
            crate::crd::PublicV1ReplacementState::new(
                &status.public_v1_topology.as_ref().unwrap().configuration_id,
                reports[1].storage_incarnation.clone(),
            )
            .map_err(anyhow::Error::msg)?,
        );
        ensure!(!Core::settled(&replacement, &reports, 3));
        let topology = status.public_v1_topology.as_ref().unwrap();
        let configuration = topology.configuration().map_err(anyhow::Error::msg)?;
        let predecessor = topology
            .sessions
            .iter()
            .find(|session| session.role == ReplicaRole::ActiveSecondary)
            .unwrap()
            .clone();
        let mut fresh = predecessor.clone();
        fresh.process_session_id = ProcessSessionId::new("detached-renewal-session");
        fresh.replicator_creation_id = ReplicatorCreationId::new("detached-renewal-creation");
        fresh.role = ReplicaRole::None;
        let epoch = Epoch::new(
            configuration.epoch.data_loss_number,
            configuration.epoch.configuration_number + 1,
        );
        let required = ProcessSessionRenewalRequired {
            operation_id: OperationId::new("detached-pending-renewal"),
            reason: ProcessSessionRenewalReason::ActiveSecondaryReplacement,
            resource_uid: reports[0].resource_uid.clone(),
            history: reports[0].history.clone(),
            previous_epoch: configuration.epoch,
            renewal_epoch: epoch,
            predecessor: predecessor.clone(),
            replacement: Some(fresh),
            surviving_sessions: topology
                .sessions
                .iter()
                .filter(|session| session.identity != predecessor.identity)
                .cloned()
                .collect(),
            fresh_configuration: ConfigurationDescriptor::new(
                epoch,
                configuration.primary_id,
                configuration.members.clone(),
                configuration.write_quorum,
            ),
            predecessor_contained: true,
            data_loss: None,
        };
        let renewal =
            PublicV1SessionRenewalStatus::required(required).map_err(anyhow::Error::msg)?;
        let mut pending_renewal = status.clone();
        pending_renewal.public_v1_session_renewal = Some(
            crate::crd::PublicV1SessionRenewalState::from_renewal(&renewal)
                .map_err(anyhow::Error::msg)?,
        );
        ensure!(!Core::settled(&pending_renewal, &reports, 3));
        for (field, value) in [
            ("processSessionId", serde_json::json!("stale-session")),
            ("replicatorCreationId", serde_json::json!("stale-creation")),
            (
                "storageIncarnation",
                serde_json::to_value(&reports[2].storage_incarnation)?,
            ),
            ("role", serde_json::json!("none")),
            ("writeStatus", serde_json::json!("granted")),
            ("readStatus", serde_json::json!("granted")),
        ] {
            let mut altered = reports.clone();
            let mut report = serde_json::to_value(&altered[1])?;
            report[field] = value;
            altered[1] = serde_json::from_value(report)?;
            ensure!(
                !Core::settled(&status, &altered, 3),
                "accepted stale {field}"
            );
        }
        let mut altered = reports.clone();
        altered
            .iter_mut()
            .find(|report| report.role == ReplicaRole::Primary)
            .unwrap()
            .write_status = AccessStatus::NotPrimary;
        ensure!(!Core::settled(&status, &altered, 3));
        for index in 0..reports.len() {
            let mut altered = reports.clone();
            altered[index].progress.current_progress = ProgressQueryResult::NotReady;
            ensure!(
                !Core::settled(&status, &altered, 3)
                    && !Core::configurations_settled(&status, &altered, &evidence),
                "unavailable native progress accepted for {:?}",
                altered[index].role
            );
        }
        let mut outsider = reports[0].clone();
        outsider.replica.instance_id = ReplicaInstanceId::new("excluded-predecessor");
        outsider.write_status = AccessStatus::Granted;
        altered = reports.clone();
        altered.push(outsider.clone());
        ensure!(!Core::settled(&status, &altered, 3));
        outsider.write_status = AccessStatus::NotPrimary;
        outsider.read_status = AccessStatus::NotPrimary;
        altered = reports.clone();
        altered.push(outsider.clone());
        ensure!(
            !Core::settled(&status, &altered, 3),
            "nonwriting unauthorized role accepted"
        );
        outsider.role = ReplicaRole::None;
        outsider.read_status = AccessStatus::Granted;
        altered = reports.clone();
        altered.push(outsider.clone());
        ensure!(
            !Core::settled(&status, &altered, 3),
            "excluded reader accepted"
        );
        let uids = reports
            .iter()
            .map(|report| report.replica.instance_id.to_string())
            .chain(std::iter::once(outsider.replica.instance_id.to_string()))
            .collect::<Vec<_>>();
        ensure!(!Core::observations_complete(&uids, &reports));
        ensure!(
            !Core::observations_complete(&uids, &reports[..2]),
            "two admitted observations hid unreachable predecessor"
        );
        altered = reports;
        for report in &mut altered {
            report.completed_operations.clear();
        }
        ensure!(!Core::settled(&status, &altered, 3));
        Ok(())
    }
    .await;
    harness.finish(result).await
}

#[test]
fn redaction_keeps_nonsecret_schema_and_removes_credentials() {
    let mut value = serde_json::json!({
        "kind":"Pod","spec":{"automountServiceAccountToken":false,"env":[{"name":"KUBERIC_AGENT_BEARER_TOKEN","value":"do-not-log"}]},
        "secret":{"kind":"Secret","data":{"bearer-token":"hidden"},"stringData":{"password":"hidden"}}
    });
    super::cluster::redact(&mut value);
    assert_eq!(value["spec"]["automountServiceAccountToken"], false);
    assert!(value["spec"]["env"][0].get("value").is_none());
    assert!(value["secret"].get("data").is_none());
    assert!(value["secret"].get("stringData").is_none());
    assert!(!value.to_string().contains("do-not-log"));
}

#[tokio::test]
async fn invalid_builder_is_inert_and_classified() {
    let result = TestCluster::builder(EmptyFactory)
        .namespace("invalid.namespace")
        .start()
        .await;
    assert_eq!(
        result.err().unwrap().kind(),
        ErrorKind::InvalidConfiguration
    );
    for builder in [
        TestCluster::builder(EmptyFactory).initial_replicas(0),
        TestCluster::builder(EmptyFactory).initial_replicas(4),
        TestCluster::builder(EmptyFactory).scenario_timeout(std::time::Duration::MAX),
        TestCluster::builder(EmptyFactory).teardown_timeout(std::time::Duration::MAX),
    ] {
        assert_eq!(
            builder.start().await.err().unwrap().kind(),
            ErrorKind::InvalidConfiguration
        );
    }

    let result = TestCluster::builder(EmptyFactory)
        .scenario_timeout(std::time::Duration::ZERO)
        .start()
        .await;
    assert_eq!(
        result.err().unwrap().kind(),
        ErrorKind::InvalidConfiguration
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_singleton_scale_uses_real_build_and_preserves_default_controls() -> super::Result<()>
{
    let mut cluster = TestCluster::builder(EmptyFactory)
        .initial_replicas(1)
        .start()
        .await?;
    let result: super::Result<()> = async {
        if cluster.ordinary_integration_unavailable().await? {
            return Ok(());
        }
        let singleton = cluster.settle_members(1).await?;
        assert_eq!(singleton.members().count(), 1);
        let before = cluster.observe().await?;
        assert_eq!(before.set().spec.replicas, 1);
        for invalid in [0, 4] {
            assert_eq!(
                cluster.scale_to(invalid).await.unwrap_err().kind(),
                super::ErrorKind::InvalidConfiguration
            );
        }
        assert_eq!(cluster.observe().await?.set().spec.replicas, 1);
        cluster.scale_to(2).await?;
        let pause = cluster
            .pause_at(super::Barrier::completion(
                kuberic_runtime::protocol::public_operations::PublicV1CallKind::Build,
            ))
            .await?;
        assert!(matches!(
            pause
                .evidence()
                .operation()
                .command()
                .unwrap()
                .intent
                .program,
            Some(kuberic_runtime::protocol::public_operations::PublicOperationProgram::Build(_))
        ));
        assert_eq!(
            cluster.scale_to(1).await.unwrap_err().kind(),
            super::ErrorKind::InvalidConfiguration
        );
        cluster.resume(pause).await?;
        assert_eq!(cluster.settle_members(2).await?.members().count(), 2);
        Ok(())
    }
    .await;
    cluster.finish(result).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_cross_cluster_stopped_and_observation_references_are_checked() -> Result<()> {
    let mut first = TestCluster::builder(EmptyFactory).start().await?;
    let mut second = TestCluster::builder(EmptyFactory).start().await?;
    let result: Result<()> = async {
        let (a, b) = tokio::try_join!(
            first.ordinary_integration_unavailable(),
            second.ordinary_integration_unavailable()
        )?;
        if a || b {
            ensure!(a && b, "identical factories disagreed on M1 engine support");
            return Ok(());
        }
        let (a, b) = tokio::try_join!(first.settle(), second.settle())?;
        assert_eq!(
            first.service(b.primary()).await.err().unwrap().kind(),
            ErrorKind::InvalidReference
        );
        let resource = second
            .observe()
            .await?
            .resources()
            .iter()
            .find(|resource| resource.reference().kind() == ResourceKind::Pod)
            .unwrap()
            .reference()
            .clone();
        assert_eq!(
            first.hold_deletion(&resource).await.err().unwrap().kind(),
            ErrorKind::InvalidReference
        );
        let hold = second.hold_deletion(&resource).await?;
        assert_eq!(
            second.hold_deletion(&resource).await.err().unwrap().kind(),
            ErrorKind::FaultAlreadyActive
        );
        drop(hold);
        drop(second.hold_deletion(&resource).await?);
        let stopped = a.secondaries().next().unwrap().clone();
        first.stop(&stopped).await?;
        assert_eq!(
            first.service(&stopped).await.err().unwrap().kind(),
            ErrorKind::InvalidReference
        );
        assert_eq!(
            first.isolate(&[stopped]).await.err().unwrap().kind(),
            ErrorKind::InvalidReference
        );
        Ok(())
    }
    .await;
    let first_result = first.finish(result).await;
    let second_result = second.finish(Ok::<_, anyhow::Error>(())).await;
    first_result?;
    second_result?;
    Ok(())
}

struct FailingDrainFactory;
#[async_trait::async_trait]
impl ApplicationFactory for FailingDrainFactory {
    type Service = EmptyApplication;
    async fn create(
        &self,
        context: ReplicaContext,
    ) -> super::Result<CreatedApplication<EmptyApplication>> {
        EmptyFactory.create(context).await
    }
    async fn shutdown_and_drain(&self, service: Arc<EmptyApplication>) -> super::Result<()> {
        service.drain().await?;
        Err(TestError::with_source(
            ErrorKind::Cleanup,
            "intentional worker failure",
            anyhow::anyhow!("worker-source"),
        ))
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn finalization_preserves_scenario_and_every_application_drain_error() -> Result<()> {
    let mut cluster = TestCluster::builder(FailingDrainFactory).start().await?;
    if !cluster.ordinary_integration_unavailable().await? {
        cluster.settle().await?;
    }
    let error = cluster
        .finish(Err(anyhow::anyhow!("scenario-source")))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Cleanup);
    assert_eq!(error.sources().count(), 4);
    assert!(
        error
            .sources()
            .any(|source| source.to_string() == "scenario-source")
    );
    let diagnostics = error.diagnostics().context("diagnostics missing")?;
    assert!(std::path::Path::new(diagnostics["retainedStorage"].as_str().unwrap()).is_dir());
    assert_eq!(diagnostics["pendingTaskOwners"], 0);
    assert!(!format!("{error:?}").contains("worker-source"));
    Ok(())
}

struct PendingFactory(Arc<std::sync::atomic::AtomicBool>);
struct ConstructionGuard(Arc<std::sync::atomic::AtomicBool>);
impl Drop for ConstructionGuard {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Release);
    }
}
#[async_trait::async_trait]
impl ApplicationFactory for PendingFactory {
    type Service = EmptyApplication;
    async fn create(
        &self,
        _: ReplicaContext,
    ) -> super::Result<CreatedApplication<EmptyApplication>> {
        let _guard = ConstructionGuard(self.0.clone());
        std::future::pending().await
    }
    async fn shutdown_and_drain(&self, _: Arc<EmptyApplication>) -> super::Result<()> {
        Ok(())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_deadline_cancels_construction_and_preserves_owned_storage() -> Result<()> {
    let cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut cluster = TestCluster::builder(PendingFactory(cancelled.clone()))
        .scenario_timeout(std::time::Duration::from_millis(400))
        .teardown_timeout(std::time::Duration::from_secs(2))
        .start()
        .await?;
    let error = cluster
        .settle()
        .await
        .err()
        .context("pending construction unexpectedly settled")?;
    assert_eq!(error.kind(), ErrorKind::Deadline);
    let error = cluster.finish(Err(error)).await.unwrap_err();
    assert!(cancelled.load(std::sync::atomic::Ordering::Acquire));
    assert!(error.diagnostics().is_some());
    Ok(())
}

#[tokio::test]
async fn probe_never_drives_and_uses_the_remaining_shared_deadline() -> Result<()> {
    let mut cluster = TestCluster::builder(EmptyFactory)
        .scenario_timeout(std::time::Duration::from_millis(80))
        .start()
        .await?;
    assert!(cluster.observe().await?.set().status.is_none());
    cluster
        .probe(async {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Ok::<_, anyhow::Error>(())
        })
        .await?;
    assert!(cluster.observe().await?.set().status.is_none());
    let error = cluster
        .probe(std::future::pending::<anyhow::Result<()>>())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Deadline);
    assert!(cluster.finish(Err(error)).await.is_err());
    Ok(())
}

struct BlockingDrainFactory {
    entered: Arc<std::sync::atomic::AtomicUsize>,
    completed: Arc<std::sync::atomic::AtomicUsize>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait::async_trait]
impl ApplicationFactory for BlockingDrainFactory {
    type Service = EmptyApplication;
    async fn create(
        &self,
        context: ReplicaContext,
    ) -> super::Result<CreatedApplication<EmptyApplication>> {
        EmptyFactory.create(context).await
    }
    async fn shutdown_and_drain(&self, service: Arc<EmptyApplication>) -> super::Result<()> {
        service.drain().await?;
        let released = self.release.notified();
        tokio::pin!(released);
        released.as_mut().enable();
        self.entered
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        released.await;
        self.completed
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timed_out_drain_keeps_application_and_route_owners_until_they_join() -> Result<()> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let entered = Arc::new(AtomicUsize::new(0));
    let completed = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let mut cluster = TestCluster::builder(BlockingDrainFactory {
        entered: entered.clone(),
        completed: completed.clone(),
        release: release.clone(),
    })
    .teardown_timeout(std::time::Duration::from_millis(500))
    .start()
    .await?;
    let uids = if cluster.ordinary_integration_unavailable().await? {
        cluster
            .observe()
            .await?
            .replicas()
            .map(|reference| reference.instance_id().to_owned())
            .collect::<Vec<_>>()
    } else {
        cluster
            .settle()
            .await?
            .members()
            .map(|reference| reference.instance_id().to_owned())
            .collect::<Vec<_>>()
    };
    let error = cluster
        .finish(Ok::<_, anyhow::Error>(()))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Cleanup);
    let diagnostics = error.diagnostics().context("diagnostics missing")?;
    assert_eq!(diagnostics["pendingTaskOwners"], 3);
    assert!(std::path::Path::new(diagnostics["retainedStorage"].as_str().unwrap()).is_dir());
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while entered.load(Ordering::Acquire) != 3 {
            tokio::task::yield_now().await;
        }
    })
    .await?;
    for uid in &uids {
        assert!(
            kuberic_runtime::replicator::TestingTcpRoute::register(uid.clone(), |_, endpoint| {
                Box::pin(async move { Ok(endpoint) })
            })
            .is_err()
        );
    }
    release.notify_waiters();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while completed.load(Ordering::Acquire) != 3 {
            tokio::task::yield_now().await;
        }
        for uid in uids {
            loop {
                if let Ok(route) = kuberic_runtime::replicator::TestingTcpRoute::register(
                    uid.clone(),
                    |_, endpoint| Box::pin(async move { Ok(endpoint) }),
                ) {
                    drop(route);
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
    })
    .await?;
    Ok(())
}

#[test]
fn storage_bindings_reject_traversal_and_symlink_escape() -> Result<()> {
    let parent = super::cluster::storage_parent()?;
    std::fs::create_dir_all(&parent)?;
    let root = tempfile::tempdir_in(parent)?;
    let owned = root.path().join("owned");
    std::fs::create_dir(&owned)?;
    let application = CreatedApplication::new(
        Arc::new(EmptyApplication::default()),
        kuberic_runtime::host::ApplicationStorageState::FreshEmpty,
    )
    .with_storage_paths([owned.join("../escape")]);
    assert_eq!(
        application.validate(&owned).unwrap_err().kind(),
        ErrorKind::InvalidConfiguration
    );
    let alias = root.path().join("owned-alias");
    std::os::unix::fs::symlink(&owned, &alias)?;
    for path in [alias.join("provider"), owned.join("provider")] {
        CreatedApplication::new(
            Arc::new(EmptyApplication::default()),
            kuberic_runtime::host::ApplicationStorageState::FreshEmpty,
        )
        .with_storage_paths([path])
        .validate(&alias)?;
    }
    #[cfg(unix)]
    {
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside)?;
        std::os::unix::fs::symlink(&outside, owned.join("link"))?;
        let application = CreatedApplication::new(
            Arc::new(EmptyApplication::default()),
            kuberic_runtime::host::ApplicationStorageState::FreshEmpty,
        )
        .with_storage_paths([owned.join("link").join("provider")]);
        assert_eq!(
            application.validate(&owned).unwrap_err().kind(),
            ErrorKind::InvalidConfiguration
        );
    }
    Ok(())
}

struct RejectedProviderFactory(Arc<std::sync::Mutex<Option<std::path::PathBuf>>>);
#[async_trait::async_trait]
impl ApplicationFactory for RejectedProviderFactory {
    type Service = EmptyApplication;
    async fn create(
        &self,
        context: ReplicaContext,
    ) -> super::Result<CreatedApplication<EmptyApplication>> {
        let path = context.application_root().join("invalid-provider");
        std::fs::write(&path, b"unsupported-provider-format")?;
        *self.0.lock().unwrap() = Some(path);
        Err(TestError::with_source(
            ErrorKind::Application,
            "provider format rejected",
            std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid-provider-source"),
        ))
    }
    async fn shutdown_and_drain(&self, _: Arc<EmptyApplication>) -> super::Result<()> {
        panic!("failed construction returned no application to drain")
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_provider_preserves_error_source_and_storage_unchanged() -> Result<()> {
    let path = Arc::new(std::sync::Mutex::new(None));
    let mut cluster = TestCluster::builder(RejectedProviderFactory(path.clone()))
        .start()
        .await?;
    let error = cluster
        .settle()
        .await
        .err()
        .context("rejected provider unexpectedly settled")?;
    assert_eq!(error.kind(), ErrorKind::Application);
    let error = cluster.finish(Err(error)).await.unwrap_err();
    assert!(error.diagnostics().is_some());
    let path = path
        .lock()
        .unwrap()
        .clone()
        .context("factory did not run")?;
    assert_eq!(std::fs::read(path)?, b"unsupported-provider-format");
    let mut source = std::error::Error::source(&error);
    let mut found = false;
    while let Some(error) = source {
        found |= error.to_string().contains("invalid-provider-source");
        source = error.source();
    }
    assert!(found, "factory source was not preserved");
    Ok(())
}

struct PanickingFactory(Arc<std::sync::atomic::AtomicBool>);
#[async_trait::async_trait]
impl ApplicationFactory for PanickingFactory {
    type Service = EmptyApplication;
    async fn create(
        &self,
        _: ReplicaContext,
    ) -> super::Result<CreatedApplication<EmptyApplication>> {
        let _guard = ConstructionGuard(self.0.clone());
        panic!("factory-panic-source")
    }
    async fn shutdown_and_drain(&self, _: Arc<EmptyApplication>) -> super::Result<()> {
        Ok(())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn factory_panic_is_fatal_preserves_source_and_releases_constructor_resources() -> Result<()>
{
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut cluster = TestCluster::builder(PanickingFactory(released.clone()))
        .start()
        .await?;
    let error = cluster
        .settle()
        .await
        .err()
        .context("panicking constructor unexpectedly settled")?;
    assert_eq!(error.kind(), ErrorKind::Fatal);
    assert!(released.load(std::sync::atomic::Ordering::Acquire));
    let error = cluster.finish(Err(error)).await.unwrap_err();
    let mut source = std::error::Error::source(&error);
    let mut found = false;
    while let Some(error) = source {
        found |= error.is::<tokio::task::JoinError>();
        source = error.source();
    }
    assert!(found, "supervisor panic source was discarded");
    Ok(())
}

struct PanickingDrainFactory;
#[async_trait::async_trait]
impl ApplicationFactory for PanickingDrainFactory {
    type Service = EmptyApplication;
    async fn create(
        &self,
        context: ReplicaContext,
    ) -> super::Result<CreatedApplication<EmptyApplication>> {
        EmptyFactory.create(context).await
    }
    async fn shutdown_and_drain(&self, service: Arc<EmptyApplication>) -> super::Result<()> {
        service.drain().await?;
        panic!("application-drain-panic-source")
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_application_drain_panic_remains_a_cleanup_source() -> Result<()> {
    let mut cluster = TestCluster::builder(PanickingDrainFactory).start().await?;
    if !cluster.ordinary_integration_unavailable().await? {
        cluster.settle().await?;
    }
    let error = cluster
        .finish(Ok::<_, anyhow::Error>(()))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Cleanup);
    assert_eq!(
        error
            .sources()
            .filter(|source| source.is::<tokio::task::JoinError>())
            .count(),
        3
    );
    Ok(())
}

struct CountingDrainFactory(Arc<std::sync::atomic::AtomicUsize>);
#[async_trait::async_trait]
impl ApplicationFactory for CountingDrainFactory {
    type Service = EmptyApplication;
    async fn create(
        &self,
        context: ReplicaContext,
    ) -> super::Result<CreatedApplication<EmptyApplication>> {
        EmptyFactory.create(context).await
    }
    async fn shutdown_and_drain(&self, service: Arc<EmptyApplication>) -> super::Result<()> {
        service.drain().await?;
        self.0.fetch_add(1, std::sync::atomic::Ordering::Release);
        Ok(())
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn early_drop_requests_owned_async_drain_without_claiming_completion() -> Result<()> {
    let completed = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut cluster = TestCluster::builder(CountingDrainFactory(completed.clone()))
        .start()
        .await?;
    let uids = if cluster.ordinary_integration_unavailable().await? {
        cluster
            .observe()
            .await?
            .replicas()
            .map(|reference| reference.instance_id().to_owned())
            .collect::<Vec<_>>()
    } else {
        cluster
            .settle()
            .await?
            .members()
            .map(|reference| reference.instance_id().to_owned())
            .collect::<Vec<_>>()
    };
    drop(cluster);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while completed.load(std::sync::atomic::Ordering::Acquire) != 3 {
            tokio::task::yield_now().await;
        }
        for uid in uids {
            loop {
                if let Ok(route) = kuberic_runtime::replicator::TestingTcpRoute::register(
                    uid.clone(),
                    |_, endpoint| Box::pin(async move { Ok(endpoint) }),
                ) {
                    drop(route);
                    break;
                }
                tokio::task::yield_now().await;
            }
        }
    })
    .await?;
    Ok(())
}
