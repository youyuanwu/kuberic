use std::collections::BTreeSet;
use std::sync::Mutex;

use ed25519_dalek::SigningKey;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::{Container, Pod, PodSpec, Secret};
use kube::CustomResourceExt;
use kuberic_controller::crd::KubericSet;
use kuberic_controller::evaluator::test_bridge::{
    PUBLIC_V1_AUTHORIZATION_GENERATION_LABEL, PUBLIC_V1_AUTHORIZATION_KEY_ID,
    PUBLIC_V1_AUTHORIZATION_PUBLIC_KEY, PUBLIC_V1_AUTHORIZATION_RESOURCE_UID_LABEL,
    PUBLIC_V1_AUTHORIZATION_SECRET_TYPE, PUBLIC_V1_AUTHORIZATION_SEED_KEY,
    PublicV1AuthorizationApi, PublicV1AuthorizationDispatch, PublicV1AuthorizationEvidence,
    PublicV1AuthorizationIntentSlot, PublicV1AuthorizationRequest,
    PublicV1AuthorizationSecretOwner, execute_public_v1_authorization,
    generate_public_v1_authorization_secret, plan_public_v1_authorization,
    public_v1_authorization_secret_binding,
    validate_pods_do_not_reference_public_v1_authorization_secret,
    validate_public_v1_authorization_secret,
};
use kuberic_runtime::protocol::authorization::{
    AuthorizationVerificationKey, BuildAuthorizationClaimsV1, BuildAuthorizationEndpointV1,
    PeerSessionAuthorizationClaimsV1, PeerSessionEndpointV1,
};
use kuberic_runtime::protocol::public_operations::{
    PublicOperationPreviewIdentity, PublicV1AcceptedStatus, PublicV1AuthorizationClaims,
    PublicV1AuthorizationCleanupDisposition, PublicV1AuthorizationIntentKind,
    PublicV1AuthorizationIntentStage,
};
use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationId, Epoch, HistoryContext, HistoryId, OperationId, PodUid,
    ProcessSessionId, PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaInstanceId, ReplicaRole, ReplicaStorageIdentity, ReplicaStorageIncarnationContext,
    ResourceUid, StorageGeneration, derive_bootstrap_history, derive_history_initialization_id,
};

#[derive(Default)]
struct FakeApi {
    status: Mutex<Option<PublicV1AcceptedStatus>>,
    secret: Mutex<Option<Secret>>,
    pods: Mutex<Vec<Pod>>,
    dispatches: Mutex<Vec<PublicV1AuthorizationDispatch>>,
    lose_next_dispatch_response: Mutex<bool>,
}

#[async_trait::async_trait]
impl PublicV1AuthorizationApi for FakeApi {
    async fn observe_public_v1_authorization_status(
        &self,
    ) -> Result<Option<PublicV1AcceptedStatus>, String> {
        Ok(self.status.lock().unwrap().clone())
    }

    async fn observe_public_v1_authorization_secret(
        &self,
        name: &str,
    ) -> Result<Option<Secret>, String> {
        Ok(self
            .secret
            .lock()
            .unwrap()
            .as_ref()
            .filter(|secret| secret.metadata.name.as_deref() == Some(name))
            .cloned())
    }

    async fn observe_public_v1_authorization_pods(&self) -> Result<Vec<Pod>, String> {
        Ok(self.pods.lock().unwrap().clone())
    }

    async fn create_public_v1_authorization_secret(
        &self,
        mut secret: Secret,
    ) -> Result<(), String> {
        let mut current = self.secret.lock().unwrap();
        if current.is_some() {
            return Err("authorization Secret already exists".into());
        }
        secret.metadata.uid = Some("authorization-secret-uid".into());
        secret.metadata.resource_version = Some("17".into());
        *current = Some(secret);
        Ok(())
    }

    async fn persist_public_v1_authorization_status(
        &self,
        expected: &PublicV1AcceptedStatus,
        next: &PublicV1AcceptedStatus,
    ) -> Result<(), String> {
        let mut status = self.status.lock().unwrap();
        if status.as_ref() != Some(expected) {
            return Err("conditional authorization status replacement was stale".into());
        }
        *status = Some(next.clone());
        Ok(())
    }

    async fn dispatch_public_v1_authorization(
        &self,
        expected: &PublicV1AcceptedStatus,
        dispatch: &PublicV1AuthorizationDispatch,
    ) -> Result<(), String> {
        if self.status.lock().unwrap().as_ref() != Some(expected) {
            return Err("authorization dispatch was stale".into());
        }
        dispatch.validate()?;
        self.dispatches.lock().unwrap().push(dispatch.clone());
        if std::mem::take(&mut *self.lose_next_dispatch_response.lock().unwrap()) {
            return Err("simulated response loss".into());
        }
        Ok(())
    }
}

fn accepted() -> PublicV1AcceptedStatus {
    let resource_uid = ResourceUid::new("resource-1");
    let generation = PublicProtocolGeneration::new(42);
    let initialization = derive_history_initialization_id(&resource_uid, generation);
    PublicV1AcceptedStatus {
        identity: PublicOperationPreviewIdentity::public_v1(generation.value()),
        history_initialization_id: initialization.clone(),
        history: derive_bootstrap_history(&resource_uid, generation, &initialization),
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        authorization: None,
        extensions: Default::default(),
    }
}

fn owner() -> PublicV1AuthorizationSecretOwner {
    PublicV1AuthorizationSecretOwner {
        name: "db".into(),
        resource_uid: ResourceUid::new("resource-1"),
        generation: PublicProtocolGeneration::new(42),
    }
}

fn identity(id: i64) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("pod-{id}")),
        agent_generation: AgentGeneration::new(format!("agent-{id}")),
    }
}

fn storage(id: i64) -> ReplicaStorageIncarnationContext {
    ReplicaStorageIncarnationContext {
        replica: identity(id),
        storage: ReplicaStorageIdentity::Volatile {
            identity: format!("volatile-{id}"),
        },
        storage_generation: StorageGeneration::new(format!("storage-{id}")),
    }
}

fn golden_owner() -> PublicV1AuthorizationSecretOwner {
    PublicV1AuthorizationSecretOwner {
        name: "golden".into(),
        resource_uid: ResourceUid::new("resource-123"),
        generation: PublicProtocolGeneration::new(11),
    }
}

fn golden_secret(owner: &PublicV1AuthorizationSecretOwner) -> Secret {
    let seed = [7; 32];
    let signing_key = SigningKey::from_bytes(&seed);
    let verification_key =
        AuthorizationVerificationKey::from_bytes(signing_key.verifying_key().to_bytes()).unwrap();
    let mut secret = generate_public_v1_authorization_secret(owner).unwrap();
    secret.data = Some(std::collections::BTreeMap::from([
        (
            PUBLIC_V1_AUTHORIZATION_SEED_KEY.into(),
            ByteString(seed.to_vec()),
        ),
        (
            PUBLIC_V1_AUTHORIZATION_PUBLIC_KEY.into(),
            ByteString(verification_key.to_bytes().to_vec()),
        ),
        (
            PUBLIC_V1_AUTHORIZATION_KEY_ID.into(),
            ByteString(verification_key.key_id().to_bytes().to_vec()),
        ),
    ]));
    secret.metadata.uid = Some("golden-secret-uid".into());
    secret.metadata.resource_version = Some("1".into());
    secret
}

fn golden_accepted(
    owner: &PublicV1AuthorizationSecretOwner,
    secret: &Secret,
) -> PublicV1AcceptedStatus {
    let initialization = derive_history_initialization_id(&owner.resource_uid, owner.generation);
    let mut accepted = PublicV1AcceptedStatus {
        identity: PublicOperationPreviewIdentity::public_v1(owner.generation.value()),
        history_initialization_id: initialization,
        history: HistoryContext {
            resource_uid: owner.resource_uid.clone(),
            protocol_generation: owner.generation,
            history_id: HistoryId::new("history-abc"),
            data_loss_number: 4,
        },
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        authorization: None,
        extensions: Default::default(),
    };
    accepted.authorization_mut().secret_binding =
        Some(public_v1_authorization_secret_binding(secret, owner).unwrap());
    accepted
}

fn golden_replica(id: i64, name: &str) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: ReplicaInstanceId::new(format!("{name}-instance")),
        agent_generation: AgentGeneration::new(format!("{name}-agent")),
    }
}

fn golden_build_endpoint(name: &str, id: i64, role: ReplicaRole) -> BuildAuthorizationEndpointV1 {
    BuildAuthorizationEndpointV1 {
        storage_incarnation: ReplicaStorageIncarnationContext {
            replica: golden_replica(id, name),
            storage: ReplicaStorageIdentity::Persistent {
                pod_uid: PodUid::new(format!("{name}-pod")),
                pvc_uid: PvcUid::new(format!("{name}-pvc")),
            },
            storage_generation: StorageGeneration::new(format!("{name}-storage-3")),
        },
        process_session_id: ProcessSessionId::new(format!("{name}-session-5")),
        replication_address: format!("https://10.0.0.{id}:7000"),
        role,
    }
}

fn golden_peer_endpoint(name: &str, id: i64, role: ReplicaRole) -> PeerSessionEndpointV1 {
    PeerSessionEndpointV1 {
        replica: golden_replica(id, name),
        process_session_id: ProcessSessionId::new(format!("{name}-session-5")),
        replication_address: format!("https://10.0.0.{id}:7000"),
        role,
    }
}

async fn controller_signed_envelope(
    owner: &PublicV1AuthorizationSecretOwner,
    secret: &Secret,
    accepted: PublicV1AcceptedStatus,
    request: PublicV1AuthorizationRequest,
) -> kuberic_runtime::protocol::public_operations::PublicV1AuthorizationEnvelope {
    let api = FakeApi {
        status: Mutex::new(Some(accepted)),
        secret: Mutex::new(Some(secret.clone())),
        ..Default::default()
    };
    let current = api.status.lock().unwrap().clone().unwrap();
    let prepare =
        plan_public_v1_authorization(&current, owner, Some(secret), Some(&request), None).unwrap();
    execute_public_v1_authorization(&api, prepare)
        .await
        .unwrap();
    let prepared = api.status.lock().unwrap().clone().unwrap();
    let sign =
        plan_public_v1_authorization(&prepared, owner, Some(secret), Some(&request), None).unwrap();
    execute_public_v1_authorization(&api, sign).await.unwrap();
    api.status
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .authorization()
        .unwrap()
        .intents[0]
        .envelope
        .clone()
        .unwrap()
}

fn build_request(accepted: &PublicV1AcceptedStatus) -> PublicV1AuthorizationRequest {
    build_request_with_revision(accepted, 1)
}

fn build_request_with_revision(
    accepted: &PublicV1AcceptedStatus,
    revision: u64,
) -> PublicV1AuthorizationRequest {
    let binding = accepted
        .authorization()
        .unwrap()
        .secret_binding
        .as_ref()
        .unwrap();
    let operation_id = OperationId::new(format!("build-operation-{revision}"));
    PublicV1AuthorizationRequest {
        slot: PublicV1AuthorizationIntentSlot::Build,
        operation_id: operation_id.clone(),
        revision,
        claims: PublicV1AuthorizationClaims::build(BuildAuthorizationClaimsV1 {
            resource_uid: accepted.history.resource_uid.clone(),
            protocol_generation: accepted.history.protocol_generation,
            key_id: binding.key_id,
            history: accepted.history.clone(),
            epoch: Epoch::new(0, 1),
            operation_id,
            build_id: OperationId::new(format!("build-{revision}")),
            source: BuildAuthorizationEndpointV1 {
                storage_incarnation: storage(1),
                process_session_id: ProcessSessionId::new("session-1"),
                replication_address: "primary:50052".into(),
                role: ReplicaRole::Primary,
            },
            target: BuildAuthorizationEndpointV1 {
                storage_incarnation: storage(2),
                process_session_id: ProcessSessionId::new("session-2"),
                replication_address: "secondary:50052".into(),
                role: ReplicaRole::IdleSecondary,
            },
        }),
        cleanup_disposition: PublicV1AuthorizationCleanupDisposition::RetireAfterObservation,
    }
}

#[tokio::test]
async fn public_v1_authorization_secret_material_is_controller_only() {
    let owner = owner();
    let api = FakeApi {
        status: Mutex::new(Some(accepted())),
        pods: Mutex::new(vec![Pod {
            metadata: kube::core::ObjectMeta {
                name: Some("db-1".into()),
                ..Default::default()
            },
            spec: Some(PodSpec {
                automount_service_account_token: Some(false),
                containers: vec![Container {
                    name: "application".into(),
                    command: Some(vec!["kuberic-agent".into()]),
                    ..Default::default()
                }],
                ..Default::default()
            }),
            ..Default::default()
        }]),
        ..Default::default()
    };

    let initial = api.status.lock().unwrap().clone().unwrap();
    let create = plan_public_v1_authorization(&initial, &owner, None, None, None).unwrap();
    execute_public_v1_authorization(&api, create).await.unwrap();

    let secret = api.secret.lock().unwrap().clone().unwrap();
    assert_eq!(
        secret.type_.as_deref(),
        Some(PUBLIC_V1_AUTHORIZATION_SECRET_TYPE)
    );
    assert_eq!(secret.immutable, Some(true));
    let owner_reference = secret.metadata.owner_references.as_ref().unwrap();
    assert_eq!(owner_reference.len(), 1);
    assert_eq!(owner_reference[0].uid, owner.resource_uid.to_string());
    assert_eq!(owner_reference[0].name, owner.name);
    assert_eq!(owner_reference[0].controller, Some(true));
    assert_eq!(
        secret.metadata.labels.as_ref().unwrap(),
        &std::collections::BTreeMap::from([
            (
                PUBLIC_V1_AUTHORIZATION_RESOURCE_UID_LABEL.into(),
                owner.resource_uid.to_string(),
            ),
            (
                PUBLIC_V1_AUTHORIZATION_GENERATION_LABEL.into(),
                owner.generation.value().to_string(),
            ),
        ])
    );
    let data = secret.data.as_ref().unwrap();
    assert_eq!(
        data.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        BTreeSet::from([
            PUBLIC_V1_AUTHORIZATION_KEY_ID,
            PUBLIC_V1_AUTHORIZATION_PUBLIC_KEY,
            PUBLIC_V1_AUTHORIZATION_SEED_KEY,
        ])
    );
    assert!(data.values().all(|value| value.0.len() == 32));
    let seed = data[PUBLIC_V1_AUTHORIZATION_SEED_KEY].0.clone();
    assert!(
        !format!(
            "{:?}",
            validate_public_v1_authorization_secret(&secret, &owner).unwrap()
        )
        .contains(&hex(&seed))
    );

    for malformed in [
        {
            let mut malformed = secret.clone();
            malformed.immutable = Some(false);
            malformed
        },
        {
            let mut malformed = secret.clone();
            malformed.metadata.owner_references.as_mut().unwrap()[0].uid = "other".into();
            malformed
        },
        {
            let mut malformed = secret.clone();
            malformed
                .data
                .as_mut()
                .unwrap()
                .get_mut(PUBLIC_V1_AUTHORIZATION_PUBLIC_KEY)
                .unwrap()
                .0[0] ^= 1;
            malformed
        },
        {
            let mut malformed = secret.clone();
            malformed
                .data
                .as_mut()
                .unwrap()
                .insert("unexpected".into(), k8s_openapi::ByteString(vec![0; 32]));
            malformed
        },
    ] {
        assert!(validate_public_v1_authorization_secret(&malformed, &owner).is_err());
    }

    validate_pods_do_not_reference_public_v1_authorization_secret(
        &api.pods.lock().unwrap(),
        &owner.secret_name(),
    )
    .unwrap();
    assert_eq!(
        api.pods.lock().unwrap()[0]
            .spec
            .as_ref()
            .unwrap()
            .automount_service_account_token,
        Some(false),
        "replica Pods must not receive Kubernetes API credentials"
    );
    let mut credentialed = api.pods.lock().unwrap()[0].clone();
    credentialed
        .spec
        .as_mut()
        .unwrap()
        .automount_service_account_token = Some(true);
    assert!(
        validate_pods_do_not_reference_public_v1_authorization_secret(
            &[credentialed],
            &owner.secret_name(),
        )
        .is_err(),
        "replica Pods with service-account credentials must fail closed"
    );
    let mut referenced = api.pods.lock().unwrap()[0].clone();
    referenced.spec.as_mut().unwrap().containers[0].command = Some(vec![owner.secret_name()]);
    assert!(
        validate_pods_do_not_reference_public_v1_authorization_secret(
            &[referenced],
            &owner.secret_name(),
        )
        .is_err()
    );

    let bind = plan_public_v1_authorization(&initial, &owner, Some(&secret), None, None).unwrap();
    execute_public_v1_authorization(&api, bind).await.unwrap();
    let bound = api.status.lock().unwrap().clone().unwrap();
    let binding = bound
        .authorization()
        .unwrap()
        .secret_binding
        .as_ref()
        .unwrap();
    assert_eq!(binding.name, owner.secret_name());
    assert_eq!(binding.uid, "authorization-secret-uid");
    assert_eq!(binding.resource_version, "17");

    assert!(
        plan_public_v1_authorization(&bound, &owner, None, None, None).is_err(),
        "bound Secret loss must close terminally"
    );
    let mut recreated = generate_public_v1_authorization_secret(&owner).unwrap();
    recreated.metadata.uid = Some("replacement-secret-uid".into());
    recreated.metadata.resource_version = Some("18".into());
    assert!(
        plan_public_v1_authorization(&bound, &owner, Some(&recreated), None, None).is_err(),
        "a bound Secret must never be regenerated"
    );
    let mut changed_version = secret.clone();
    changed_version.metadata.resource_version = Some("18".into());
    assert!(
        plan_public_v1_authorization(&bound, &owner, Some(&changed_version), None, None,).is_err()
    );

    let request = build_request(&bound);
    let prepare =
        plan_public_v1_authorization(&bound, &owner, Some(&secret), Some(&request), None).unwrap();
    execute_public_v1_authorization(&api, prepare)
        .await
        .unwrap();
    let prepared = api.status.lock().unwrap().clone().unwrap();
    assert_eq!(
        prepared.authorization().unwrap().intents[0].stage,
        PublicV1AuthorizationIntentStage::Prepared
    );

    let sign = plan_public_v1_authorization(&prepared, &owner, Some(&secret), Some(&request), None)
        .unwrap();
    execute_public_v1_authorization(&api, sign).await.unwrap();
    let signed = api.status.lock().unwrap().clone().unwrap();
    let signed_intent = &signed.authorization().unwrap().intents[0];
    assert_eq!(
        signed_intent.stage,
        PublicV1AuthorizationIntentStage::Signed
    );
    assert!(signed_intent.envelope.is_some());
    assert!(!prepared.is_authorization_successor_of(&signed));

    *api.lose_next_dispatch_response.lock().unwrap() = true;
    let dispatch =
        plan_public_v1_authorization(&signed, &owner, Some(&secret), Some(&request), None).unwrap();
    assert!(
        execute_public_v1_authorization(&api, dispatch)
            .await
            .is_err()
    );
    assert_eq!(
        api.status.lock().unwrap().as_ref().unwrap(),
        &signed,
        "response loss must retain the exact Signed envelope"
    );

    let retry =
        plan_public_v1_authorization(&signed, &owner, Some(&secret), Some(&request), None).unwrap();
    execute_public_v1_authorization(&api, retry).await.unwrap();
    let dispatched_bytes = {
        let dispatches = api.dispatches.lock().unwrap();
        assert_eq!(dispatches.len(), 2);
        assert_eq!(dispatches[0], dispatches[1]);
        serde_json::to_vec(&dispatches[0].envelope).unwrap()
    };
    assert!(
        !dispatched_bytes
            .windows(seed.len())
            .any(|window| window == seed),
        "agent dispatch must not contain the Secret seed"
    );

    let dispatched = api.status.lock().unwrap().clone().unwrap();
    let intent = &dispatched.authorization().unwrap().intents[0];
    assert_eq!(intent.stage, PublicV1AuthorizationIntentStage::Dispatched);
    let evidence = PublicV1AuthorizationEvidence {
        operation_id: intent.operation_id.clone(),
        revision: intent.revision,
        kind: PublicV1AuthorizationIntentKind::Build,
        audit_digest: intent.audit_digest.unwrap(),
    };
    let observe = plan_public_v1_authorization(
        &dispatched,
        &owner,
        Some(&secret),
        Some(&request),
        Some(&evidence),
    )
    .unwrap();
    execute_public_v1_authorization(&api, observe)
        .await
        .unwrap();
    let observed = api.status.lock().unwrap().clone().unwrap();
    assert_eq!(
        observed.authorization().unwrap().intents[0].stage,
        PublicV1AuthorizationIntentStage::Observed
    );

    let retire = plan_public_v1_authorization(
        &observed,
        &owner,
        Some(&secret),
        Some(&request),
        Some(&evidence),
    )
    .unwrap();
    execute_public_v1_authorization(&api, retire).await.unwrap();
    let retired = api.status.lock().unwrap().clone().unwrap();
    assert_eq!(
        retired.authorization().unwrap().intents[0].stage,
        PublicV1AuthorizationIntentStage::Retired
    );
    let retired_intent = &retired.authorization().unwrap().intents[0];
    assert!(retired_intent.claims.is_none());
    assert!(retired_intent.canonical_claims_base64.is_none());
    assert!(retired_intent.envelope.is_none());
    assert!(retired_intent.signature.is_none());
    assert!(retired_intent.audit_digest.is_some());
    assert!(
        plan_public_v1_authorization(&retired, &owner, Some(&secret), Some(&request), None,)
            .is_err(),
        "retired predecessor authorization cannot be replayed"
    );

    let compact =
        plan_public_v1_authorization(&retired, &owner, Some(&secret), None, None).unwrap();
    execute_public_v1_authorization(&api, compact)
        .await
        .unwrap();
    assert!(
        api.status
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .authorization()
            .unwrap()
            .intents
            .is_empty(),
        "retired audit tombstones must be removed after safe retirement"
    );

    for revision in 2..=18 {
        let current = api.status.lock().unwrap().clone().unwrap();
        let request = build_request_with_revision(&current, revision);
        for expected_stage in [
            PublicV1AuthorizationIntentStage::Prepared,
            PublicV1AuthorizationIntentStage::Signed,
            PublicV1AuthorizationIntentStage::Dispatched,
        ] {
            let current = api.status.lock().unwrap().clone().unwrap();
            let plan =
                plan_public_v1_authorization(&current, &owner, Some(&secret), Some(&request), None)
                    .unwrap();
            execute_public_v1_authorization(&api, plan).await.unwrap();
            assert_eq!(
                api.status
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .authorization()
                    .unwrap()
                    .intents[0]
                    .stage,
                expected_stage
            );
        }
        let dispatched = api.status.lock().unwrap().clone().unwrap();
        let intent = &dispatched.authorization().unwrap().intents[0];
        let evidence = PublicV1AuthorizationEvidence {
            operation_id: intent.operation_id.clone(),
            revision: intent.revision,
            kind: intent.kind,
            audit_digest: intent.audit_digest.unwrap(),
        };
        for expected_stage in [
            PublicV1AuthorizationIntentStage::Observed,
            PublicV1AuthorizationIntentStage::Retired,
        ] {
            let current = api.status.lock().unwrap().clone().unwrap();
            let plan = plan_public_v1_authorization(
                &current,
                &owner,
                Some(&secret),
                Some(&request),
                Some(&evidence),
            )
            .unwrap();
            execute_public_v1_authorization(&api, plan).await.unwrap();
            assert_eq!(
                api.status
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .authorization()
                    .unwrap()
                    .intents[0]
                    .stage,
                expected_stage
            );
        }
        let retired = api.status.lock().unwrap().clone().unwrap();
        let compact =
            plan_public_v1_authorization(&retired, &owner, Some(&secret), None, None).unwrap();
        execute_public_v1_authorization(&api, compact)
            .await
            .unwrap();
        assert!(
            api.status
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .authorization()
                .unwrap()
                .intents
                .is_empty()
        );
    }
}

#[tokio::test]
async fn controller_signing_matches_runtime_golden_vectors() {
    let owner = golden_owner();
    let secret = golden_secret(&owner);
    let accepted = golden_accepted(&owner, &secret);
    let key_id = accepted
        .authorization()
        .unwrap()
        .secret_binding
        .as_ref()
        .unwrap()
        .key_id;
    let build_claims = BuildAuthorizationClaimsV1 {
        resource_uid: owner.resource_uid.clone(),
        protocol_generation: owner.generation,
        key_id,
        history: accepted.history.clone(),
        epoch: Epoch::new(4, 12),
        operation_id: OperationId::new("operation-42"),
        build_id: OperationId::new("build-77"),
        source: golden_build_endpoint("source", 1, ReplicaRole::Primary),
        target: golden_build_endpoint("target", 2, ReplicaRole::IdleSecondary),
    };
    let build_envelope = controller_signed_envelope(
        &owner,
        &secret,
        accepted.clone(),
        PublicV1AuthorizationRequest {
            slot: PublicV1AuthorizationIntentSlot::Build,
            operation_id: OperationId::new("operation-42"),
            revision: 1,
            claims: PublicV1AuthorizationClaims::build(build_claims),
            cleanup_disposition: PublicV1AuthorizationCleanupDisposition::RetireAfterObservation,
        },
    )
    .await;
    let build = build_envelope.build.unwrap();
    assert_eq!(
        hex(&build.claims.canonical_bytes().unwrap()),
        "000000256b7562657269632d6275696c642d617574686f72697a6174696f6e2d636c61696d732d7631010000000c7265736f757263652d313233000000000000000b00000020fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff5522638890000000c7265736f757263652d313233000000000000000b0000000b686973746f72792d61626300000000000000040000000000000004000000000000000c0000000c6f7065726174696f6e2d3432000000086275696c642d373700000000000000010000000f736f757263652d696e7374616e63650000000c736f757263652d6167656e74010000000a736f757263652d706f640000000a736f757263652d70766300000010736f757263652d73746f726167652d3300000010736f757263652d73657373696f6e2d350000001568747470733a2f2f31302e302e302e313a373030300100000000000000020000000f7461726765742d696e7374616e63650000000c7461726765742d6167656e74010000000a7461726765742d706f640000000a7461726765742d707663000000107461726765742d73746f726167652d33000000107461726765742d73657373696f6e2d350000001568747470733a2f2f31302e302e302e323a3730303003"
    );
    assert_eq!(
        build.signature.to_base64(),
        "o4PKbXMNCV5ODDaxPFflUJ1mt/RD7BIyisWjiJE4AF0lY1JONScnaJIyL+hqmbY74eLOc/OrXMmFySwBW7KJCg=="
    );
    assert_eq!(
        build.audit_digest().unwrap().to_base64(),
        "xUzvtO+Ef71api56PAJSPTcCtyZ0YnM/thJ0PbFSgRk="
    );

    let peer_claims = PeerSessionAuthorizationClaimsV1 {
        resource_uid: owner.resource_uid.clone(),
        protocol_generation: owner.generation,
        key_id,
        history: accepted.history.clone(),
        epoch: Epoch::new(4, 12),
        current_configuration_id: ConfigurationId::new("cfg-current-12"),
        previous_configuration_id: Some(ConfigurationId::new("cfg-previous-11")),
        primary: golden_peer_endpoint("source", 1, ReplicaRole::Primary),
        secondary: golden_peer_endpoint("target", 2, ReplicaRole::ActiveSecondary),
        configuration_revision: 19,
    };
    let peer_envelope = controller_signed_envelope(
        &owner,
        &secret,
        accepted,
        PublicV1AuthorizationRequest {
            slot: PublicV1AuthorizationIntentSlot::CurrentPeer,
            operation_id: OperationId::new("peer-operation-19"),
            revision: 19,
            claims: PublicV1AuthorizationClaims::peer(peer_claims),
            cleanup_disposition: PublicV1AuthorizationCleanupDisposition::RetainUntilSuperseded,
        },
    )
    .await;
    let peer = peer_envelope.peer.unwrap();
    assert_eq!(
        hex(&peer.claims.canonical_bytes().unwrap()),
        "0000002c6b7562657269632d706565722d73657373696f6e2d617574686f72697a6174696f6e2d636c61696d732d7631010000000c7265736f757263652d313233000000000000000b00000020fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff5522638890000000c7265736f757263652d313233000000000000000b0000000b686973746f72792d61626300000000000000040000000000000004000000000000000c0000000e6366672d63757272656e742d3132010000000f6366672d70726576696f75732d313100000000000000010000000f736f757263652d696e7374616e63650000000c736f757263652d6167656e7400000010736f757263652d73657373696f6e2d350000001568747470733a2f2f31302e302e302e313a373030300100000000000000020000000f7461726765742d696e7374616e63650000000c7461726765742d6167656e74000000107461726765742d73657373696f6e2d350000001568747470733a2f2f31302e302e302e323a37303030020000000000000013"
    );
    assert_eq!(
        peer.signature.to_base64(),
        "iiioE4mvhDgSZ3ub37GnlX21dmeZS/fu1MX3mL7/d5l+xRjHbQiiRWKhW9hFbjmy2UDnyYrvwBweGDMc01dICg=="
    );
    assert_eq!(
        peer.audit_digest().unwrap().to_base64(),
        "rIJSowF9rpWsENmfPdCUPBGUi4282BKFw8l6PsXzESI="
    );
}

#[test]
fn public_v1_authorization_status_schema_is_structural_and_bounded() {
    let schema = serde_json::to_value(KubericSet::crd()).unwrap();
    let authorization = &schema["spec"]["versions"][0]["schema"]["openAPIV3Schema"]["properties"]["status"]
        ["properties"]["publicV1Authority"]["properties"]["authorization"]["properties"];
    assert_eq!(authorization["intents"]["maxItems"], 16);
    let binding = &authorization["secretBinding"]["properties"];
    for field in [
        "name",
        "uid",
        "resourceVersion",
        "resourceUid",
        "generation",
        "keyId",
        "verificationKey",
    ] {
        assert!(binding.get(field).is_some(), "{field}");
    }
    let intent = &authorization["intents"]["items"]["properties"];
    for field in [
        "kind",
        "scope",
        "operationId",
        "revision",
        "claims",
        "canonicalClaimsBase64",
        "stage",
        "cleanupDisposition",
    ] {
        assert!(intent.get(field).is_some(), "{field}");
    }
}

#[test]
fn public_v1_authorization_tracks_current_and_previous_peer_intents_separately() {
    let owner = owner();
    let mut accepted = accepted();
    let mut secret = generate_public_v1_authorization_secret(&owner).unwrap();
    secret.metadata.uid = Some("authorization-secret-uid".into());
    secret.metadata.resource_version = Some("17".into());
    accepted.authorization_mut().secret_binding = Some(
        kuberic_controller::evaluator::test_bridge::public_v1_authorization_secret_binding(
            &secret, &owner,
        )
        .unwrap(),
    );
    let binding = accepted
        .authorization()
        .unwrap()
        .secret_binding
        .as_ref()
        .unwrap();
    let claims = |revision, previous: Option<&str>| {
        PublicV1AuthorizationClaims::peer(PeerSessionAuthorizationClaimsV1 {
            resource_uid: accepted.history.resource_uid.clone(),
            protocol_generation: accepted.history.protocol_generation,
            key_id: binding.key_id,
            history: accepted.history.clone(),
            epoch: Epoch::new(0, 1),
            current_configuration_id: ConfigurationId::new(format!("configuration-{revision}")),
            previous_configuration_id: previous.map(ConfigurationId::new),
            primary: PeerSessionEndpointV1 {
                replica: identity(1),
                process_session_id: ProcessSessionId::new("session-1"),
                replication_address: "primary:50052".into(),
                role: ReplicaRole::Primary,
            },
            secondary: PeerSessionEndpointV1 {
                replica: identity(2),
                process_session_id: ProcessSessionId::new("session-2"),
                replication_address: "secondary:50052".into(),
                role: ReplicaRole::ActiveSecondary,
            },
            configuration_revision: revision,
        })
    };
    let current = PublicV1AuthorizationRequest {
        slot: PublicV1AuthorizationIntentSlot::CurrentPeer,
        operation_id: OperationId::new("peer-current"),
        revision: 2,
        claims: claims(2, Some("configuration-1")),
        cleanup_disposition: PublicV1AuthorizationCleanupDisposition::RetainUntilSuperseded,
    };
    let kuberic_controller::evaluator::test_bridge::PublicV1AuthorizationPlan::PersistStatus {
        next,
        ..
    } = plan_public_v1_authorization(&accepted, &owner, Some(&secret), Some(&current), None)
        .unwrap()
    else {
        panic!("current peer authorization was not prepared");
    };
    let previous = PublicV1AuthorizationRequest {
        slot: PublicV1AuthorizationIntentSlot::PreviousPeer,
        operation_id: OperationId::new("peer-previous"),
        revision: 3,
        claims: claims(3, Some("configuration-2")),
        cleanup_disposition: PublicV1AuthorizationCleanupDisposition::RetainUntilSuperseded,
    };
    let kuberic_controller::evaluator::test_bridge::PublicV1AuthorizationPlan::PersistStatus {
        next,
        ..
    } = plan_public_v1_authorization(&next, &owner, Some(&secret), Some(&previous), None).unwrap()
    else {
        panic!("previous peer authorization was not prepared");
    };
    assert_eq!(
        next.authorization()
            .unwrap()
            .intents
            .iter()
            .filter(|intent| intent.scope == PublicV1AuthorizationIntentSlot::CurrentPeer)
            .count(),
        1
    );
    assert_eq!(
        next.authorization()
            .unwrap()
            .intents
            .iter()
            .filter(|intent| intent.scope == PublicV1AuthorizationIntentSlot::PreviousPeer)
            .count(),
        1
    );

    let malformed_previous = PublicV1AuthorizationRequest {
        claims: claims(4, None),
        revision: 4,
        operation_id: OperationId::new("peer-previous-without-configuration"),
        ..previous
    };
    assert!(malformed_previous.validate().is_err());
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
