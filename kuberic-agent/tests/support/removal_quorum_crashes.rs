use super::*;
use kuberic_protocol::types::{
    SecondaryRemovalEvidence, SecondaryRemovalStage, SecondaryRemovalWitness,
    SecondaryScaleDownCleanup,
};
use kuberic_runtime::application::ClientWrite;

struct Member {
    store: Arc<SqliteStore>,
    runtime: Arc<PodRuntime>,
    application: Arc<CrashState>,
    session: ProcessSessionId,
}

async fn member(
    root: &Path,
    intent: &SecondaryScaleDownIntent,
    index: usize,
    create: bool,
) -> Member {
    let local = intent.current_configuration.members[index].identity.clone();
    let path =
        SqliteStore::metadata_database_path(&root.join(format!("replica-{}", local.replica_id)));
    let mut initial = state(intent, false);
    initial.identity.local_identity = local.clone();
    initial.identity.pod_uid = PodUid::new(local.instance_id.as_str());
    initial.identity.pvc_uid = PvcUid::new(format!("pvc-{}", local.replica_id));
    initial.role = intent.current_configuration.members[index].role;
    if index != 0 {
        initial.write_status = AccessStatus::NotPrimary;
    }
    let store = Arc::new(if create {
        SqliteStore::create_authorized(&path, initial.clone()).unwrap()
    } else {
        SqliteStore::open_existing(&path, Some(&initial.identity)).unwrap()
    });
    if create {
        store
            .admit(&AdmittedAuthority {
                local_identity: local.clone(),
                transition_kind: None,
                previous_configuration: None,
                current_configuration: intent.previous_configuration.clone(),
                switchover_handoff: None,
                secondary_removal: None,
            })
            .await
            .unwrap();
    }
    let mut application = CrashState::open(crash_application_path(&path));
    application.consume_replication = true;
    let application = Arc::new(application);
    let runtime = Arc::new(PodRuntime::new(local, application.clone(), store.clone()));
    let service = kuberic_agent::service::AgentService::new(
        store.clone(),
        runtime.clone(),
        runtime.clone(),
        "token",
    )
    .unwrap();
    service.reconstruct_runtime().await.unwrap();
    Member {
        store,
        runtime,
        application,
        session: service.sessions().local_session().clone(),
    }
}

async fn action(member: &Member, action: RuntimeEffectAction) {
    let sequence = member
        .store
        .load_state()
        .await
        .unwrap()
        .next_effect_sequence;
    RuntimeAdapter::new(member.store.clone(), member.runtime.clone())
        .execute(RuntimeEffect {
            operation_id: OperationId::new(format!("quorum-effect-{sequence}")),
            sequence,
            action,
        })
        .await
        .unwrap();
}

async fn register(primary: &Member, secondary: &Member) {
    action(
        primary,
        RuntimeEffectAction::RegisterPeerSession {
            identity: secondary.runtime.snapshot().await.identity,
            session: secondary.session.clone(),
        },
    )
    .await;
}

async fn witness(
    member: &Member,
    intent: &SecondaryScaleDownIntent,
    sequence: u64,
) -> SecondaryRemovalWitness {
    let snapshot = member.runtime.snapshot().await;
    let state = member.store.load_state().await.unwrap();
    assert!(state.pending_effect.is_none());
    assert!(state.reconfiguration.is_none());
    let authority = snapshot.authority.unwrap();
    SecondaryRemovalWitness {
        resource_uid: intent.resource_uid.clone(),
        identity: snapshot.identity,
        role: snapshot.role,
        process_session_id: member.session.clone(),
        report_sequence: sequence,
        epoch: authority.current_configuration.epoch,
        previous_configuration_id: authority.previous_configuration.map(|c| c.configuration_id),
        current_configuration_id: authority.current_configuration.configuration_id,
        verified_replication_lsn: snapshot.verified_replication_lsn.unwrap(),
        write_status: snapshot.write_status,
        pending_operation_id: None,
        retained_operation_id: state
            .retained_command
            .map(|r| r.command.operation_id)
            .or_else(|| state.prepared_secondary_removal.map(|p| p.operation_id)),
    }
}

fn configuration(
    member: &ReplicaIdentity,
    evidence: &SecondaryRemovalEvidence,
    current_only: bool,
) -> EnsureConfiguration {
    let mut command = command(&evidence.preparation, current_only);
    command.operation_id = evidence.preparation.intent.command_operation_id(
        if current_only {
            SecondaryRemovalStage::CurrentOnly
        } else {
            SecondaryRemovalStage::PreviousCurrent
        },
        member,
    );
    command.local_replica_id = member.replica_id;
    command.expected_instance_id = member.instance_id.clone();
    command.expected_agent_generation = member.agent_generation.clone();
    command.secondary_removal_evidence = Some(evidence.clone());
    command
}

async fn configure(member: &Member, evidence: &SecondaryRemovalEvidence, current_only: bool) {
    Coordinator::new(member.store.clone(), member.runtime.clone())
        .ensure_configuration(configuration(
            &member.runtime.snapshot().await.identity,
            evidence,
            current_only,
        ))
        .await
        .unwrap();
}

async fn write_success(primary: &Member, secondary: &Member, id: &str) -> i64 {
    eprintln!("writing {id}");
    let write = ClientWrite {
        operation_id: OperationId::new(id),
        data: Bytes::from(format!("value-{id}")),
    };
    let pending = primary
        .runtime
        .data_plane()
        .begin_write(write.clone())
        .await
        .unwrap();
    let lsn = pending.lsn;
    let secondary_identity = secondary.runtime.snapshot().await.identity;
    let item = pending
        .replication_items
        .iter()
        .find(|item| item.receiver.as_ref() == Some(&secondary_identity.clone().into()))
        .unwrap()
        .clone();
    let authority = primary.runtime.snapshot().await.authority.unwrap();
    if authority.secondary_removal.is_some() {
        assert_eq!(
            pending.replication_items.len(),
            1,
            "excluded target receives no replication"
        );
    }
    let completion = tokio::spawn(pending.committed());
    let mut acknowledgement = secondary
        .runtime
        .data_plane()
        .receive_replication(item)
        .await
        .unwrap()
        .applied()
        .await
        .unwrap();
    eprintln!("secondary applied {id}");
    acknowledgement.receiver_session_id = secondary.session.to_string();
    if let Some(evidence) = &authority.secondary_removal {
        let mut stale = acknowledgement.clone();
        stale.receiver_session_id = evidence.reduced_write_quorum[1]
            .process_session_id
            .to_string();
        assert!(
            primary
                .runtime
                .data_plane()
                .accept_acknowledgement(stale)
                .await
                .is_err()
        );
        let mut excluded = acknowledgement.clone();
        excluded.receiver = Some(evidence.preparation.intent.target.clone().into());
        assert!(
            primary
                .runtime
                .data_plane()
                .accept_acknowledgement(excluded)
                .await
                .is_err()
        );
    }
    assert!(
        !completion.is_finished(),
        "no client success without retained secondary credit"
    );
    primary
        .runtime
        .data_plane()
        .accept_acknowledgement(acknowledgement)
        .await
        .unwrap();
    let receipt = completion.await.unwrap().unwrap();
    assert_eq!(receipt.lsn, lsn);
    for member in [primary, secondary] {
        assert_eq!(
            member.application.state.lock().unwrap().operations[&lsn],
            write.data.as_ref()
        );
    }
    let durable = primary
        .store
        .load_local_write(&write.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(durable.lsn, lsn);
    assert_eq!(durable.data, write.data);
    assert_eq!(
        durable.phase,
        kuberic_runtime_internal::authority::LocalWritePhase::Committed
    );
    lsn
}

async fn cleanup(
    primary: &Member,
    secondary: &Member,
    evidence: SecondaryRemovalEvidence,
) -> SecondaryScaleDownCleanup {
    let intent = &evidence.preparation.intent;
    let witnesses = vec![
        witness(primary, intent, 4).await,
        witness(secondary, intent, 4).await,
    ];
    assert_eq!(witnesses.len(), intent.current_policy.write_quorum as usize);
    assert!(witnesses.iter().all(|w| w.identity != intent.target));
    SecondaryScaleDownCleanup {
        evidence,
        current_only_write_quorum: witnesses,
        retirement: None,
    }
}

#[tokio::test]
#[ignore]
async fn writer() {
    tokio::time::timeout(std::time::Duration::from_secs(10), write_until_cut())
        .await
        .unwrap();
}

async fn write_until_cut() {
    let root = PathBuf::from(env::var("KUBERIC_QUORUM_PATH").unwrap());
    let boundary = env::var("KUBERIC_QUORUM_BOUNDARY").unwrap();
    let intent = fixture::intent(&[1, 2, 3], 1);
    let primary = member(&root, &intent, 0, true).await;
    eprintln!("opened primary");
    let secondary = member(&root, &intent, 1, true).await;
    eprintln!("opened secondary");
    register(&primary, &secondary).await;
    assert_eq!(
        write_success(&primary, &secondary, "before-reduction").await,
        1
    );
    let preparation = Coordinator::new(primary.store.clone(), primary.runtime.clone())
        .ensure_secondary_removal_prepared(
            fixture::prepare_command(&intent),
            primary.session.clone(),
            1,
        )
        .await
        .unwrap();
    assert_eq!(preparation.boundary_lsn, 1);
    eprintln!("prepared");
    let mut evidence = SecondaryRemovalEvidence {
        preparation,
        previous_read_quorum: vec![
            witness(&primary, &intent, 2).await,
            witness(&secondary, &intent, 2).await,
        ],
        reduced_write_quorum: Vec::new(),
    };
    assert_eq!(evidence.previous_read_quorum.len(), 2);
    configure(&secondary, &evidence, false).await;
    configure(&primary, &evidence, false).await;
    eprintln!("joint");
    evidence.reduced_write_quorum = vec![
        witness(&primary, &intent, 3).await,
        witness(&secondary, &intent, 3).await,
    ];
    register(&primary, &secondary).await;
    action(
        &primary,
        RuntimeEffectAction::ObserveSecondaryRemovalWitness(Box::new(
            evidence.reduced_write_quorum[1].clone(),
        )),
    )
    .await;
    assert!(primary.runtime.snapshot().await.catch_up_complete);
    configure(&secondary, &evidence, true).await;
    eprintln!("secondary current");
    if boundary == "current-authority-before-receipt" {
        Coordinator::new(
            primary.store.clone(),
            Arc::new(CrashRuntime {
                runtime: primary.runtime.clone(),
                boundary: "runtime-admit-authority".into(),
            }),
        )
        .ensure_configuration(configuration(&intent.primary, &evidence, true))
        .await
        .unwrap();
        panic!("current-only cut was not reached");
    }
    configure(&primary, &evidence, true).await;
    let committed = cleanup(&primary, &secondary, evidence).await;
    register(&primary, &secondary).await;
    let sequence = primary
        .store
        .load_state()
        .await
        .unwrap()
        .next_effect_sequence;
    assert_eq!(boundary, "commit-before-receipt");
    RuntimeAdapter::new(
        primary.store.clone(),
        Arc::new(CrashRuntime {
            runtime: primary.runtime.clone(),
            boundary: "runtime".into(),
        }),
    )
    .execute(RuntimeEffect {
        operation_id: OperationId::new("quorum-commit"),
        sequence,
        action: RuntimeEffectAction::AcceptSecondaryRemovalCommit(Box::new(committed)),
    })
    .await
    .unwrap();
    panic!("commit cut was not reached");
}

#[test]
fn real_three_to_two_quorum_crashes_preserve_sessions_and_successful_writes() {
    std::thread::Builder::new()
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            let boundaries = ["current-authority-before-receipt", "commit-before-receipt"];
            for boundary in boundaries {
                if env::var("KUBERIC_VERIFY_QUORUM_BOUNDARY")
                    .is_ok_and(|selected| selected != boundary)
                {
                    continue;
                }
                let directory = tempdir().unwrap();
                let output = Command::new(env::current_exe().unwrap())
                    .args([
                        "--ignored",
                        "--exact",
                        "removal_crashes::quorum_crashes::writer",
                        "--nocapture",
                    ])
                    .env("KUBERIC_QUORUM_PATH", directory.path())
                    .env("KUBERIC_QUORUM_BOUNDARY", boundary)
                    .env("RUST_MIN_STACK", "16777216")
                    .output()
                    .unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(73),
                    "{boundary}: {}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(async {
                        tokio::time::timeout(std::time::Duration::from_secs(10), async {
                            let intent = fixture::intent(&[1, 2, 3], 1);
                            let primary = member(directory.path(), &intent, 0, false).await;
                            let secondary = member(directory.path(), &intent, 1, false).await;
                            assert_ne!(
                                primary.runtime.snapshot().await.write_status,
                                AccessStatus::Granted
                            );
                            Coordinator::new(primary.store.clone(), primary.runtime.clone())
                                .resume_configuration()
                                .await
                                .unwrap();
                            let evidence = primary
                                .store
                                .load_state()
                                .await
                                .unwrap()
                                .secondary_removal_evidence
                                .unwrap();
                            assert_eq!(evidence.previous_read_quorum.len(), 2);
                            assert_eq!(evidence.reduced_write_quorum.len(), 2);
                            for witness in evidence
                                .previous_read_quorum
                                .iter()
                                .chain(&evidence.reduced_write_quorum)
                            {
                                assert_ne!(witness.identity, intent.target);
                                assert_ne!(witness.process_session_id, primary.session);
                                assert_ne!(witness.process_session_id, secondary.session);
                                assert_eq!(witness.verified_replication_lsn, 1);
                            }
                            if boundary == "commit-before-receipt" {
                                let committed = primary
                                    .store
                                    .load_secondary_removal_commit()
                                    .await
                                    .unwrap()
                                    .unwrap();
                                assert_eq!(committed.evidence, evidence);
                                assert_eq!(
                                    primary.runtime.snapshot().await.accepted_secondary_removal,
                                    Some(committed.clone())
                                );
                                assert!(primary.runtime.snapshot().await.catch_up_complete);
                                let exact = committed.current_only_write_quorum[1].clone();
                                action(
                                    &primary,
                                    RuntimeEffectAction::ObserveSecondaryRemovalWitness(Box::new(
                                        exact,
                                    )),
                                )
                                .await;
                            } else {
                                assert!(
                                    !primary.runtime.snapshot().await.catch_up_complete,
                                    "no invented secondary credit after restart"
                                );
                                register(&primary, &secondary).await;
                                let committed =
                                    cleanup(&primary, &secondary, evidence.clone()).await;
                                action(
                                    &primary,
                                    RuntimeEffectAction::AcceptSecondaryRemovalCommit(Box::new(
                                        committed,
                                    )),
                                )
                                .await;
                            }
                            register(&primary, &secondary).await;
                            let already_fresh = boundary == "current-authority-before-receipt";
                            assert_eq!(
                                primary.runtime.snapshot().await.catch_up_complete,
                                already_fresh,
                                "only an already observed fresh-session witness retains credit"
                            );
                            let stale = evidence.reduced_write_quorum[1].clone();
                            let sequence = primary
                                .store
                                .load_state()
                                .await
                                .unwrap()
                                .next_effect_sequence;
                            assert!(
                                primary
                                    .runtime
                                    .apply_effect(RuntimeEffect {
                                        operation_id: OperationId::new("stale-witness"),
                                        sequence,
                                        action: RuntimeEffectAction::ObserveSecondaryRemovalWitness(
                                            Box::new(stale)
                                        ),
                                    })
                                    .await
                                    .is_err()
                            );
                            let mut target = witness(&secondary, &intent, 5).await;
                            target.identity = intent.target.clone();
                            assert!(
                                primary
                                    .runtime
                                    .apply_effect(RuntimeEffect {
                                        operation_id: OperationId::new("target-witness"),
                                        sequence,
                                        action: RuntimeEffectAction::ObserveSecondaryRemovalWitness(
                                            Box::new(target)
                                        ),
                                    })
                                    .await
                                    .is_err()
                            );
                            assert_eq!(
                                primary.runtime.snapshot().await.catch_up_complete,
                                already_fresh
                            );
                            action(
                                &primary,
                                RuntimeEffectAction::ObserveSecondaryRemovalWitness(Box::new(
                                    witness(&secondary, &intent, 5).await,
                                )),
                            )
                            .await;
                            assert!(primary.runtime.snapshot().await.catch_up_complete);
                            action(
                                &primary,
                                RuntimeEffectAction::SetAccessStatus {
                                    read: AccessStatus::Granted,
                                    write: AccessStatus::Granted,
                                },
                            )
                            .await;
                            assert_eq!(
                                write_success(&primary, &secondary, "fresh-after-restart").await,
                                2
                            );
                            assert!(primary.store.load_local_writes().await.unwrap().is_empty());
                            for (id, lsn) in [("before-reduction", 1), ("fresh-after-restart", 2)] {
                                let write = primary
                                    .store
                                    .load_local_write(&OperationId::new(id))
                                    .await
                                    .unwrap()
                                    .unwrap();
                                assert_eq!(write.lsn, lsn);
                                assert_eq!(write.data, Bytes::from(format!("value-{id}")));
                                assert_eq!(
                                    write.phase,
                                    kuberic_runtime_internal::authority::LocalWritePhase::Committed
                                );
                            }
                            for member in [&primary, &secondary] {
                                let persisted = CrashState::open(&member.application.path);
                                assert_eq!(
                                    persisted.state.lock().unwrap().operations[&1],
                                    b"value-before-reduction"
                                );
                                assert_eq!(
                                    persisted.state.lock().unwrap().operations[&2],
                                    b"value-fresh-after-restart"
                                );
                                assert_eq!(
                                    member.store.load_state().await.unwrap().admitted_policy,
                                    Some(intent.current_policy.clone())
                                );
                                assert!(
                                    member
                                        .store
                                        .load_state()
                                        .await
                                        .unwrap()
                                        .pending_effect
                                        .is_none()
                                );
                            }
                        })
                        .await
                        .unwrap_or_else(|_| panic!("quorum recovery timed out: {boundary}"));
                    });
            }
        })
        .unwrap()
        .join()
        .unwrap();
}
