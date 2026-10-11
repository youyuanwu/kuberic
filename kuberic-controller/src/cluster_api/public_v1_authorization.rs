use std::collections::BTreeMap;
use std::fmt;

use ed25519_dalek::{Signer as _, SigningKey};
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::{Pod, Secret};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
use kuberic_runtime::protocol::authorization::{
    AuthorizationAuditDigest, AuthorizationError, AuthorizationKeyId, AuthorizationSignature,
    AuthorizationVerificationKey, BuildAuthorizationClaimsV1, BuildAuthorizationV1,
    PeerSessionAuthorizationClaimsV1, PeerSessionAuthorizationV1,
};
use kuberic_runtime::protocol::public_operations::{
    PublicV1AcceptedStatus, PublicV1AuthorizationEnvelope, PublicV1AuthorizationIntentKind,
    PublicV1AuthorizationSecretBinding,
};
use kuberic_runtime::protocol::types::{OperationId, PublicProtocolGeneration, ResourceUid};

pub const PUBLIC_V1_AUTHORIZATION_SECRET_TYPE: &str = "kuberic.io/public-v1-authorization-key";
pub const PUBLIC_V1_AUTHORIZATION_SEED_KEY: &str = "ed25519-seed";
pub const PUBLIC_V1_AUTHORIZATION_PUBLIC_KEY: &str = "ed25519-public-key";
pub const PUBLIC_V1_AUTHORIZATION_KEY_ID: &str = "key-id-sha256";
pub const PUBLIC_V1_AUTHORIZATION_RESOURCE_UID_LABEL: &str = "kuberic.io/resource-uid";
pub const PUBLIC_V1_AUTHORIZATION_GENERATION_LABEL: &str = "kuberic.io/public-v1-generation";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicV1AuthorizationSecretOwner {
    pub name: String,
    pub resource_uid: ResourceUid,
    pub generation: PublicProtocolGeneration,
}

impl PublicV1AuthorizationSecretOwner {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_empty() || self.resource_uid.is_empty() || self.generation.value() == 0 {
            return Err("public-v1 authorization Secret owner is incomplete".into());
        }
        if self.secret_name().len() > 253 {
            return Err("public-v1 authorization Secret name exceeds the Kubernetes bound".into());
        }
        Ok(())
    }

    pub fn validate_for_accepted(&self, accepted: &PublicV1AcceptedStatus) -> Result<(), String> {
        self.validate()?;
        accepted.validate_authorization().map_err(str::to_owned)?;
        if !accepted.identity.is_public_v1()
            || accepted.identity.generation != self.generation.value()
            || accepted.history.resource_uid != self.resource_uid
            || accepted.history.protocol_generation != self.generation
        {
            return Err(
                "public-v1 authorization Secret owner differs from accepted history".into(),
            );
        }
        Ok(())
    }

    pub fn secret_name(&self) -> String {
        format!(
            "{}-public-v1-authorization-{}",
            self.name,
            self.generation.value()
        )
    }

    fn owner_reference(&self) -> OwnerReference {
        OwnerReference {
            api_version: "operator.kuberic.io/v1alpha1".into(),
            kind: "KubericSet".into(),
            name: self.name.clone(),
            uid: self.resource_uid.to_string(),
            block_owner_deletion: Some(true),
            controller: Some(true),
        }
    }

    fn labels(&self) -> BTreeMap<String, String> {
        BTreeMap::from([
            (
                PUBLIC_V1_AUTHORIZATION_RESOURCE_UID_LABEL.into(),
                self.resource_uid.to_string(),
            ),
            (
                PUBLIC_V1_AUTHORIZATION_GENERATION_LABEL.into(),
                self.generation.value().to_string(),
            ),
        ])
    }
}

pub struct PublicV1AuthorizationSecretMaterial {
    signing_key: ControllerAuthorizationSigningKey,
}

impl fmt::Debug for PublicV1AuthorizationSecretMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("PublicV1AuthorizationSecretMaterial(<redacted>)")
    }
}

impl PublicV1AuthorizationSecretMaterial {
    pub fn verification_key(&self) -> AuthorizationVerificationKey {
        self.signing_key.verification_key()
    }

    pub fn key_id(&self) -> AuthorizationKeyId {
        self.signing_key.key_id()
    }

    pub(crate) fn signing_key(&self) -> &ControllerAuthorizationSigningKey {
        &self.signing_key
    }
}

pub(crate) struct ControllerAuthorizationSigningKey(SigningKey);

impl ControllerAuthorizationSigningKey {
    fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }

    pub(crate) fn verification_key(&self) -> AuthorizationVerificationKey {
        AuthorizationVerificationKey::from_bytes(self.0.verifying_key().to_bytes())
            .expect("Ed25519 signing key always has a valid verification key")
    }

    pub(crate) fn key_id(&self) -> AuthorizationKeyId {
        self.verification_key().key_id()
    }

    pub(crate) fn sign_build_authorization(
        &self,
        claims: BuildAuthorizationClaimsV1,
    ) -> Result<BuildAuthorizationV1, AuthorizationError> {
        if claims.key_id != self.key_id() {
            return Err(AuthorizationError::KeyIdMismatch);
        }
        let signature =
            AuthorizationSignature::from_bytes(self.0.sign(&claims.signing_bytes()?).to_bytes());
        BuildAuthorizationV1::from_signature(claims, signature)
    }

    pub(crate) fn sign_peer_session_authorization(
        &self,
        claims: PeerSessionAuthorizationClaimsV1,
    ) -> Result<PeerSessionAuthorizationV1, AuthorizationError> {
        if claims.key_id != self.key_id() {
            return Err(AuthorizationError::KeyIdMismatch);
        }
        let signature =
            AuthorizationSignature::from_bytes(self.0.sign(&claims.signing_bytes()?).to_bytes());
        PeerSessionAuthorizationV1::from_signature(claims, signature)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublicV1AuthorizationDispatch {
    pub operation_id: OperationId,
    pub revision: u64,
    pub kind: PublicV1AuthorizationIntentKind,
    pub envelope: PublicV1AuthorizationEnvelope,
    pub audit_digest: AuthorizationAuditDigest,
}

impl PublicV1AuthorizationDispatch {
    pub fn validate(&self) -> Result<(), String> {
        if self.operation_id.is_empty()
            || self.revision == 0
            || self.kind != self.envelope.kind()
            || self.envelope.audit_digest().map_err(str::to_owned)? != self.audit_digest
        {
            return Err("public-v1 authorization dispatch is incomplete or inconsistent".into());
        }
        Ok(())
    }
}

pub fn generate_public_v1_authorization_secret(
    owner: &PublicV1AuthorizationSecretOwner,
) -> Result<Secret, String> {
    owner.validate()?;
    let mut seed = [0_u8; 32];
    getrandom::fill(&mut seed)
        .map_err(|_| "operating system CSPRNG failed to generate authorization key".to_string())?;
    Ok(public_v1_authorization_secret_from_seed(owner, seed))
}

fn public_v1_authorization_secret_from_seed(
    owner: &PublicV1AuthorizationSecretOwner,
    seed: [u8; 32],
) -> Secret {
    let signing_key = ControllerAuthorizationSigningKey::from_seed(seed);
    let verification_key = signing_key.verification_key();
    let key_id = signing_key.key_id();
    Secret {
        metadata: kube::core::ObjectMeta {
            name: Some(owner.secret_name()),
            labels: Some(owner.labels()),
            owner_references: Some(vec![owner.owner_reference()]),
            ..Default::default()
        },
        immutable: Some(true),
        type_: Some(PUBLIC_V1_AUTHORIZATION_SECRET_TYPE.into()),
        data: Some(BTreeMap::from([
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
                ByteString(key_id.to_bytes().to_vec()),
            ),
        ])),
        ..Default::default()
    }
}

pub fn validate_public_v1_authorization_secret(
    secret: &Secret,
    owner: &PublicV1AuthorizationSecretOwner,
) -> Result<PublicV1AuthorizationSecretMaterial, String> {
    owner.validate()?;
    let expected_owner = owner.owner_reference();
    let expected_labels = owner.labels();
    if secret.metadata.name.as_deref() != Some(owner.secret_name().as_str())
        || secret.type_.as_deref() != Some(PUBLIC_V1_AUTHORIZATION_SECRET_TYPE)
        || secret.immutable != Some(true)
        || secret.metadata.owner_references.as_deref()
            != Some(std::slice::from_ref(&expected_owner))
        || secret.metadata.labels.as_ref() != Some(&expected_labels)
        || secret.string_data.is_some()
    {
        return Err(
            "public-v1 authorization Secret metadata, owner, or immutability is invalid".into(),
        );
    }
    let data = secret
        .data
        .as_ref()
        .ok_or_else(|| "public-v1 authorization Secret data is missing".to_string())?;
    let expected_keys = [
        PUBLIC_V1_AUTHORIZATION_KEY_ID,
        PUBLIC_V1_AUTHORIZATION_PUBLIC_KEY,
        PUBLIC_V1_AUTHORIZATION_SEED_KEY,
    ];
    if data.len() != expected_keys.len() || expected_keys.iter().any(|key| !data.contains_key(*key))
    {
        return Err("public-v1 authorization Secret contains unexpected data keys".into());
    }
    let seed = exact_secret_bytes(data, PUBLIC_V1_AUTHORIZATION_SEED_KEY)?;
    let public_key = exact_secret_bytes(data, PUBLIC_V1_AUTHORIZATION_PUBLIC_KEY)?;
    let key_id = exact_secret_bytes(data, PUBLIC_V1_AUTHORIZATION_KEY_ID)?;
    let signing_key = ControllerAuthorizationSigningKey::from_seed(seed);
    if signing_key.verification_key().to_bytes() != public_key
        || signing_key.key_id().to_bytes() != key_id
    {
        return Err("public-v1 authorization Secret key material is mismatched".into());
    }
    Ok(PublicV1AuthorizationSecretMaterial { signing_key })
}

fn exact_secret_bytes(
    data: &BTreeMap<String, ByteString>,
    key: &'static str,
) -> Result<[u8; 32], String> {
    let bytes = &data
        .get(key)
        .ok_or_else(|| format!("public-v1 authorization Secret lacks {key}"))?
        .0;
    bytes
        .as_slice()
        .try_into()
        .map_err(|_| format!("public-v1 authorization Secret {key} must contain exactly 32 bytes"))
}

pub fn public_v1_authorization_secret_binding(
    secret: &Secret,
    owner: &PublicV1AuthorizationSecretOwner,
) -> Result<PublicV1AuthorizationSecretBinding, String> {
    let material = validate_public_v1_authorization_secret(secret, owner)?;
    let binding = PublicV1AuthorizationSecretBinding {
        name: owner.secret_name(),
        uid: secret
            .metadata
            .uid
            .clone()
            .filter(|uid| !uid.is_empty())
            .ok_or_else(|| "observed public-v1 authorization Secret lacks a UID".to_string())?,
        resource_version: secret
            .metadata
            .resource_version
            .clone()
            .filter(|resource_version| !resource_version.is_empty())
            .ok_or_else(|| {
                "observed public-v1 authorization Secret lacks a resourceVersion".to_string()
            })?,
        resource_uid: owner.resource_uid.clone(),
        generation: owner.generation,
        key_id: material.key_id(),
        verification_key: material.verification_key(),
    };
    binding.validate().map_err(str::to_owned)?;
    Ok(binding)
}

pub fn validate_public_v1_authorization_binding(
    secret: &Secret,
    owner: &PublicV1AuthorizationSecretOwner,
    binding: &PublicV1AuthorizationSecretBinding,
) -> Result<PublicV1AuthorizationSecretMaterial, String> {
    let material = validate_public_v1_authorization_secret(secret, owner)?;
    let observed = public_v1_authorization_secret_binding(secret, owner)?;
    if &observed != binding {
        return Err(
            "observed public-v1 authorization Secret differs from the accepted binding".into(),
        );
    }
    Ok(material)
}

pub fn validate_pods_do_not_reference_public_v1_authorization_secret(
    pods: &[Pod],
    secret_name: &str,
) -> Result<(), String> {
    if secret_name.is_empty() {
        return Err("public-v1 authorization Secret name is empty".into());
    }
    for pod in pods {
        if pod
            .spec
            .as_ref()
            .is_none_or(|spec| spec.automount_service_account_token != Some(false))
        {
            return Err(format!(
                "Pod {} can receive Kubernetes service-account credentials",
                pod.metadata.name.as_deref().unwrap_or("<unknown>")
            ));
        }
        if pod
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|annotations| {
                annotations
                    .iter()
                    .any(|(key, value)| key.contains(secret_name) || value.contains(secret_name))
            })
            || pod
                .spec
                .as_ref()
                .and_then(|spec| serde_json::to_value(spec).ok())
                .is_some_and(|spec| json_contains(&spec, secret_name))
        {
            return Err(format!(
                "Pod {} references controller-only authorization Secret",
                pod.metadata.name.as_deref().unwrap_or("<unknown>")
            ));
        }
    }
    Ok(())
}

fn json_contains(value: &serde_json::Value, needle: &str) -> bool {
    match value {
        serde_json::Value::String(value) => value.contains(needle),
        serde_json::Value::Array(values) => values.iter().any(|value| json_contains(value, needle)),
        serde_json::Value::Object(values) => values
            .iter()
            .any(|(key, value)| key.contains(needle) || json_contains(value, needle)),
        _ => false,
    }
}

#[async_trait::async_trait]
pub trait PublicV1AuthorizationApi: Send + Sync {
    async fn observe_public_v1_authorization_status(
        &self,
    ) -> Result<Option<PublicV1AcceptedStatus>, String>;

    async fn observe_public_v1_authorization_secret(
        &self,
        name: &str,
    ) -> Result<Option<Secret>, String>;

    async fn observe_public_v1_authorization_pods(&self) -> Result<Vec<Pod>, String>;

    async fn create_public_v1_authorization_secret(&self, secret: Secret) -> Result<(), String>;

    async fn persist_public_v1_authorization_status(
        &self,
        expected: &PublicV1AcceptedStatus,
        next: &PublicV1AcceptedStatus,
    ) -> Result<(), String>;

    async fn dispatch_public_v1_authorization(
        &self,
        expected: &PublicV1AcceptedStatus,
        dispatch: &PublicV1AuthorizationDispatch,
    ) -> Result<(), String>;
}
