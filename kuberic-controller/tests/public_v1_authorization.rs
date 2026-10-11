use std::collections::BTreeSet;
use std::sync::Mutex;

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
    validate_pods_do_not_reference_public_v1_authorization_secret,
    validate_public_v1_authorization_secret,
};
use kuberic_runtime::protocol::authorization::{
    BuildAuthorizationClaimsV1, BuildAuthorizationEndpointV1, PeerSessionAuthorizationClaimsV1,
    PeerSessionEndpointV1,
};
use kuberic_runtime::protocol::public_operations::{
    PublicOperationPreviewIdentity, PublicV1AcceptedStatus, PublicV1AuthorizationClaims,
    PublicV1AuthorizationCleanupDisposition, PublicV1AuthorizationIntentKind,
    PublicV1AuthorizationIntentStage,
};
use kuberic_runtime::protocol::types::{
    AgentGeneration, ConfigurationId, Epoch, OperationId, ProcessSessionId,
    PublicProtocolGeneration, ReplicaId, ReplicaIdentity, ReplicaInstanceId, ReplicaRole,
    ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ResourceUid, StorageGeneration,
    derive_bootstrap_history, derive_history_initialization_id,
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

fn build_request(accepted: &PublicV1AcceptedStatus) -> PublicV1AuthorizationRequest {
    let binding = accepted
        .authorization()
        .unwrap()
        .secret_binding
        .as_ref()
        .unwrap();
    let operation_id = OperationId::new("build-operation");
    PublicV1AuthorizationRequest {
        slot: PublicV1AuthorizationIntentSlot::Build,
        operation_id: operation_id.clone(),
        revision: 1,
        claims: PublicV1AuthorizationClaims::build(BuildAuthorizationClaimsV1 {
            resource_uid: accepted.history.resource_uid.clone(),
            protocol_generation: accepted.history.protocol_generation,
            key_id: binding.key_id,
            history: accepted.history.clone(),
            epoch: Epoch::new(0, 1),
            operation_id,
            build_id: OperationId::new("build-2"),
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
    assert!(
        plan_public_v1_authorization(&retired, &owner, Some(&secret), Some(&request), None,)
            .is_err(),
        "retired predecessor authorization cannot be replayed"
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
