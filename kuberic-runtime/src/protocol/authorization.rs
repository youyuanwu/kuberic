use std::fmt;

use base64::prelude::{BASE64_STANDARD, Engine as _};
use ed25519_dalek::{Signature, VerifyingKey};
#[cfg(test)]
use ed25519_dalek::{Signer as _, SigningKey};
use schemars::JsonSchema;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::protocol::types::{
    ConfigurationId, Epoch, HistoryContext, OperationId, ProcessSessionId,
    PublicProtocolGeneration, ReplicaIdentity, ReplicaRole, ReplicaStorageIdentity,
    ReplicaStorageIncarnationContext, ResourceUid, StorageGeneration,
};

const AUTHORIZATION_VERSION_V1: u8 = 1;
const BUILD_CLAIMS_DOMAIN_V1: &str = "kuberic-build-authorization-claims-v1";
const BUILD_ENVELOPE_DOMAIN_V1: &str = "kuberic-build-authorization-envelope-v1";
const PEER_CLAIMS_DOMAIN_V1: &str = "kuberic-peer-session-authorization-claims-v1";
const PEER_ENVELOPE_DOMAIN_V1: &str = "kuberic-peer-session-authorization-envelope-v1";
const MAX_CANONICAL_FIELD_LENGTH: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AuthorizationError {
    #[error("{field} must contain exactly {expected} bytes, got {actual}")]
    InvalidLength {
        field: &'static str,
        expected: usize,
        actual: usize,
    },
    #[error("{field} is not canonical base64")]
    InvalidBase64 { field: &'static str },
    #[error("{field} exceeds the canonical length limit")]
    FieldTooLong { field: &'static str },
    #[error("{field} is not valid UTF-8")]
    InvalidUtf8 { field: &'static str },
    #[error("unexpected {context} domain")]
    UnknownDomain { context: &'static str },
    #[error("unsupported {context} version {version}")]
    UnknownVersion { context: &'static str, version: u8 },
    #[error("unknown {field} tag {tag}")]
    UnknownTag { field: &'static str, tag: u8 },
    #[error("canonical {field} is truncated")]
    Truncated { field: &'static str },
    #[error("canonical authorization contains trailing bytes")]
    TrailingBytes,
    #[error("duplicate {field}")]
    Duplicate { field: &'static str },
    #[error("invalid authorization claims: {0}")]
    InvalidClaims(&'static str),
    #[error("authorization key ID does not match the verification key")]
    KeyIdMismatch,
    #[error("invalid Ed25519 public key")]
    InvalidPublicKey,
    #[error("invalid Ed25519 signature")]
    InvalidSignature,
    #[error("noncanonical {0} encoding")]
    NonCanonicalEncoding(&'static str),
    #[error("{0} payload does not match its canonical re-encoding")]
    PayloadReencodingMismatch(&'static str),
}

fn encode_base64(bytes: &[u8]) -> String {
    BASE64_STANDARD.encode(bytes)
}

fn decode_base64<const N: usize>(
    encoded: &str,
    field: &'static str,
) -> Result<[u8; N], AuthorizationError> {
    let decoded = BASE64_STANDARD
        .decode(encoded)
        .map_err(|_| AuthorizationError::InvalidBase64 { field })?;
    if encode_base64(&decoded) != encoded {
        return Err(AuthorizationError::InvalidBase64 { field });
    }
    decoded
        .try_into()
        .map_err(|decoded: Vec<u8>| AuthorizationError::InvalidLength {
            field,
            expected: N,
            actual: decoded.len(),
        })
}

macro_rules! canonical_base64_value {
    ($name:ident, $size:expr, $field:literal) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
        #[schemars(transparent)]
        pub struct $name(#[schemars(with = "String")] [u8; $size]);

        impl $name {
            pub const fn from_bytes(bytes: [u8; $size]) -> Self {
                Self(bytes)
            }

            pub const fn to_bytes(self) -> [u8; $size] {
                self.0
            }

            pub fn as_bytes(&self) -> &[u8; $size] {
                &self.0
            }

            pub fn to_base64(self) -> String {
                encode_base64(&self.0)
            }

            pub fn from_base64(encoded: &str) -> Result<Self, AuthorizationError> {
                decode_base64(encoded, $field).map(Self)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_tuple(stringify!($name))
                    .field(&encode_base64(&self.0))
                    .finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&encode_base64(&self.0))
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&encode_base64(&self.0))
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let encoded = String::deserialize(deserializer)?;
                Self::from_base64(&encoded).map_err(D::Error::custom)
            }
        }
    };
}

canonical_base64_value!(AuthorizationKeyId, 32, "authorization key ID");
canonical_base64_value!(AuthorizationSignature, 64, "authorization signature");
canonical_base64_value!(AuthorizationAuditDigest, 32, "authorization audit digest");

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(transparent)]
pub struct AuthorizationVerificationKey(#[schemars(with = "String")] [u8; 32]);

pub type AuthorizationPublicKey = AuthorizationVerificationKey;

impl AuthorizationVerificationKey {
    pub fn from_bytes(bytes: [u8; 32]) -> Result<Self, AuthorizationError> {
        VerifyingKey::from_bytes(&bytes)
            .map(|_| Self(bytes))
            .map_err(|_| AuthorizationError::InvalidPublicKey)
    }

    pub const fn to_bytes(self) -> [u8; 32] {
        self.0
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    pub fn to_base64(self) -> String {
        encode_base64(&self.0)
    }

    pub fn from_base64(encoded: &str) -> Result<Self, AuthorizationError> {
        Self::from_bytes(decode_base64(encoded, "authorization public key")?)
    }

    pub fn key_id(self) -> AuthorizationKeyId {
        AuthorizationKeyId::from_bytes(Sha256::digest(self.0).into())
    }

    pub fn verify_build_authorization(
        self,
        authorization: &BuildAuthorizationV1,
    ) -> Result<(), AuthorizationError> {
        authorization.verify(&self)
    }

    pub fn verify_peer_session_authorization(
        self,
        authorization: &PeerSessionAuthorizationV1,
    ) -> Result<(), AuthorizationError> {
        authorization.verify(&self)
    }

    fn dalek(self) -> Result<VerifyingKey, AuthorizationError> {
        VerifyingKey::from_bytes(&self.0).map_err(|_| AuthorizationError::InvalidPublicKey)
    }
}

impl fmt::Debug for AuthorizationVerificationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("AuthorizationVerificationKey")
            .field(&encode_base64(&self.0))
            .finish()
    }
}

impl fmt::Display for AuthorizationVerificationKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&encode_base64(&self.0))
    }
}

impl Serialize for AuthorizationVerificationKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&encode_base64(&self.0))
    }
}

impl<'de> Deserialize<'de> for AuthorizationVerificationKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let encoded = String::deserialize(deserializer)?;
        Self::from_base64(&encoded).map_err(D::Error::custom)
    }
}

#[cfg(test)]
pub(crate) struct AuthorizationSigningKey(SigningKey);

#[cfg(test)]
impl AuthorizationSigningKey {
    pub(crate) fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }

    pub(crate) fn verification_key(&self) -> AuthorizationVerificationKey {
        AuthorizationVerificationKey(self.0.verifying_key().to_bytes())
    }

    pub(crate) fn key_id(&self) -> AuthorizationKeyId {
        self.verification_key().key_id()
    }

    pub(crate) fn sign_build_authorization(
        &self,
        claims: BuildAuthorizationClaimsV1,
    ) -> Result<BuildAuthorizationV1, AuthorizationError> {
        claims.validate()?;
        if claims.key_id != self.key_id() {
            return Err(AuthorizationError::KeyIdMismatch);
        }
        let signing_bytes = build_signing_bytes(&claims)?;
        let signature = self.0.sign(&signing_bytes).to_bytes();
        Ok(BuildAuthorizationV1 {
            claims,
            signature: AuthorizationSignature::from_bytes(signature),
        })
    }

    pub(crate) fn sign_peer_session_authorization(
        &self,
        claims: PeerSessionAuthorizationClaimsV1,
    ) -> Result<PeerSessionAuthorizationV1, AuthorizationError> {
        claims.validate()?;
        if claims.key_id != self.key_id() {
            return Err(AuthorizationError::KeyIdMismatch);
        }
        let signing_bytes = peer_signing_bytes(&claims)?;
        let signature = self.0.sign(&signing_bytes).to_bytes();
        Ok(PeerSessionAuthorizationV1 {
            claims,
            signature: AuthorizationSignature::from_bytes(signature),
        })
    }
}

#[cfg(test)]
impl fmt::Debug for AuthorizationSigningKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AuthorizationSigningKey(<redacted>)")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildAuthorizationEndpointV1 {
    pub storage_incarnation: ReplicaStorageIncarnationContext,
    pub process_session_id: ProcessSessionId,
    pub replication_address: String,
    pub role: ReplicaRole,
}

impl BuildAuthorizationEndpointV1 {
    pub fn replica(&self) -> &ReplicaIdentity {
        &self.storage_incarnation.replica
    }

    fn validate(&self, endpoint: &'static str) -> Result<(), AuthorizationError> {
        self.storage_incarnation
            .validate()
            .map_err(AuthorizationError::InvalidClaims)?;
        if self.process_session_id.is_empty() {
            return Err(AuthorizationError::InvalidClaims(match endpoint {
                "source" => "source process session is empty",
                _ => "target process session is empty",
            }));
        }
        if self.replication_address.is_empty() {
            return Err(AuthorizationError::InvalidClaims(match endpoint {
                "source" => "source replication address is empty",
                _ => "target replication address is empty",
            }));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildAuthorizationClaimsV1 {
    pub resource_uid: ResourceUid,
    pub protocol_generation: PublicProtocolGeneration,
    pub key_id: AuthorizationKeyId,
    pub history: HistoryContext,
    pub epoch: Epoch,
    pub operation_id: OperationId,
    pub build_id: OperationId,
    pub source: BuildAuthorizationEndpointV1,
    pub target: BuildAuthorizationEndpointV1,
}

impl BuildAuthorizationClaimsV1 {
    pub fn validate(&self) -> Result<(), AuthorizationError> {
        validate_common_claims(
            &self.resource_uid,
            self.protocol_generation,
            &self.history,
            self.epoch,
        )?;
        if self.operation_id.is_empty() {
            return Err(AuthorizationError::InvalidClaims(
                "build operation ID is empty",
            ));
        }
        if self.build_id.is_empty() {
            return Err(AuthorizationError::InvalidClaims("build ID is empty"));
        }
        self.source.validate("source")?;
        self.target.validate("target")?;
        if self.source.role != ReplicaRole::Primary {
            return Err(AuthorizationError::InvalidClaims(
                "build source role must be Primary",
            ));
        }
        if self.target.role != ReplicaRole::IdleSecondary {
            return Err(AuthorizationError::InvalidClaims(
                "build target role must be IdleSecondary",
            ));
        }
        if self.source.replica() == self.target.replica() {
            return Err(AuthorizationError::Duplicate {
                field: "build endpoint replica identity",
            });
        }
        if self.source.storage_incarnation == self.target.storage_incarnation {
            return Err(AuthorizationError::Duplicate {
                field: "build endpoint storage incarnation",
            });
        }
        if self.source.process_session_id == self.target.process_session_id {
            return Err(AuthorizationError::Duplicate {
                field: "build endpoint process session",
            });
        }
        if self.source.replication_address == self.target.replication_address {
            return Err(AuthorizationError::Duplicate {
                field: "build endpoint replication address",
            });
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, AuthorizationError> {
        encode_build_claims(self)
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>, AuthorizationError> {
        build_signing_bytes(self)
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, AuthorizationError> {
        self.canonical_bytes()
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, AuthorizationError> {
        decode_build_claims(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildAuthorizationV1 {
    pub claims: BuildAuthorizationClaimsV1,
    pub signature: AuthorizationSignature,
}

pub type BuildAuthorizationEnvelopeV1 = BuildAuthorizationV1;

impl BuildAuthorizationV1 {
    pub fn from_signature(
        claims: BuildAuthorizationClaimsV1,
        signature: AuthorizationSignature,
    ) -> Result<Self, AuthorizationError> {
        claims.validate()?;
        Ok(Self { claims, signature })
    }

    pub fn verify(
        &self,
        verification_key: &AuthorizationVerificationKey,
    ) -> Result<(), AuthorizationError> {
        self.claims.validate()?;
        if self.claims.key_id != verification_key.key_id() {
            return Err(AuthorizationError::KeyIdMismatch);
        }
        verification_key
            .dalek()?
            .verify_strict(
                &build_signing_bytes(&self.claims)?,
                &Signature::from_bytes(self.signature.as_bytes()),
            )
            .map_err(|_| AuthorizationError::InvalidSignature)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, AuthorizationError> {
        self.claims.validate()?;
        let mut bytes = build_signing_bytes(&self.claims)?;
        bytes.extend_from_slice(self.signature.as_bytes());
        Ok(bytes)
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, AuthorizationError> {
        self.canonical_bytes()
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, AuthorizationError> {
        decode_build_envelope(bytes)
    }

    pub fn audit_digest(&self) -> Result<AuthorizationAuditDigest, AuthorizationError> {
        Ok(AuthorizationAuditDigest::from_bytes(
            Sha256::digest(self.canonical_bytes()?).into(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeerSessionEndpointV1 {
    pub replica: ReplicaIdentity,
    pub process_session_id: ProcessSessionId,
    pub replication_address: String,
    pub role: ReplicaRole,
}

impl PeerSessionEndpointV1 {
    fn validate(&self, endpoint: &'static str) -> Result<(), AuthorizationError> {
        if self.replica.replica_id.value() <= 0
            || self.replica.instance_id.is_empty()
            || self.replica.agent_generation.is_empty()
        {
            return Err(AuthorizationError::InvalidClaims(match endpoint {
                "primary" => "primary replica identity is incomplete",
                _ => "secondary replica identity is incomplete",
            }));
        }
        if self.process_session_id.is_empty() {
            return Err(AuthorizationError::InvalidClaims(match endpoint {
                "primary" => "primary process session is empty",
                _ => "secondary process session is empty",
            }));
        }
        if self.replication_address.is_empty() {
            return Err(AuthorizationError::InvalidClaims(match endpoint {
                "primary" => "primary replication address is empty",
                _ => "secondary replication address is empty",
            }));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeerSessionAuthorizationClaimsV1 {
    pub resource_uid: ResourceUid,
    pub protocol_generation: PublicProtocolGeneration,
    pub key_id: AuthorizationKeyId,
    pub history: HistoryContext,
    pub epoch: Epoch,
    pub current_configuration_id: ConfigurationId,
    pub previous_configuration_id: Option<ConfigurationId>,
    pub primary: PeerSessionEndpointV1,
    pub secondary: PeerSessionEndpointV1,
    pub configuration_revision: u64,
}

impl PeerSessionAuthorizationClaimsV1 {
    pub fn validate(&self) -> Result<(), AuthorizationError> {
        validate_common_claims(
            &self.resource_uid,
            self.protocol_generation,
            &self.history,
            self.epoch,
        )?;
        if self.current_configuration_id.is_empty() {
            return Err(AuthorizationError::InvalidClaims(
                "current configuration ID is empty",
            ));
        }
        if let Some(previous) = &self.previous_configuration_id {
            if previous.is_empty() {
                return Err(AuthorizationError::InvalidClaims(
                    "previous configuration ID is empty",
                ));
            }
            if previous == &self.current_configuration_id {
                return Err(AuthorizationError::Duplicate {
                    field: "peer configuration ID",
                });
            }
        }
        if self.configuration_revision == 0 {
            return Err(AuthorizationError::InvalidClaims(
                "configuration revision must be positive",
            ));
        }
        self.primary.validate("primary")?;
        self.secondary.validate("secondary")?;
        if self.primary.role != ReplicaRole::Primary {
            return Err(AuthorizationError::InvalidClaims(
                "peer primary role must be Primary",
            ));
        }
        if self.secondary.role != ReplicaRole::ActiveSecondary {
            return Err(AuthorizationError::InvalidClaims(
                "peer secondary role must be ActiveSecondary",
            ));
        }
        if self.primary.replica == self.secondary.replica {
            return Err(AuthorizationError::Duplicate {
                field: "peer endpoint replica identity",
            });
        }
        if self.primary.process_session_id == self.secondary.process_session_id {
            return Err(AuthorizationError::Duplicate {
                field: "peer endpoint process session",
            });
        }
        if self.primary.replication_address == self.secondary.replication_address {
            return Err(AuthorizationError::Duplicate {
                field: "peer endpoint replication address",
            });
        }
        Ok(())
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, AuthorizationError> {
        encode_peer_claims(self)
    }

    pub fn signing_bytes(&self) -> Result<Vec<u8>, AuthorizationError> {
        peer_signing_bytes(self)
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, AuthorizationError> {
        self.canonical_bytes()
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, AuthorizationError> {
        decode_peer_claims(bytes)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PeerSessionAuthorizationV1 {
    pub claims: PeerSessionAuthorizationClaimsV1,
    pub signature: AuthorizationSignature,
}

pub type PeerSessionAuthorizationEnvelopeV1 = PeerSessionAuthorizationV1;

impl PeerSessionAuthorizationV1 {
    pub fn from_signature(
        claims: PeerSessionAuthorizationClaimsV1,
        signature: AuthorizationSignature,
    ) -> Result<Self, AuthorizationError> {
        claims.validate()?;
        Ok(Self { claims, signature })
    }

    pub fn verify(
        &self,
        verification_key: &AuthorizationVerificationKey,
    ) -> Result<(), AuthorizationError> {
        self.claims.validate()?;
        if self.claims.key_id != verification_key.key_id() {
            return Err(AuthorizationError::KeyIdMismatch);
        }
        verification_key
            .dalek()?
            .verify_strict(
                &peer_signing_bytes(&self.claims)?,
                &Signature::from_bytes(self.signature.as_bytes()),
            )
            .map_err(|_| AuthorizationError::InvalidSignature)
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, AuthorizationError> {
        self.claims.validate()?;
        let mut bytes = peer_signing_bytes(&self.claims)?;
        bytes.extend_from_slice(self.signature.as_bytes());
        Ok(bytes)
    }

    pub fn to_canonical_bytes(&self) -> Result<Vec<u8>, AuthorizationError> {
        self.canonical_bytes()
    }

    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, AuthorizationError> {
        decode_peer_envelope(bytes)
    }

    pub fn audit_digest(&self) -> Result<AuthorizationAuditDigest, AuthorizationError> {
        Ok(AuthorizationAuditDigest::from_bytes(
            Sha256::digest(self.canonical_bytes()?).into(),
        ))
    }
}

fn validate_common_claims(
    resource_uid: &ResourceUid,
    protocol_generation: PublicProtocolGeneration,
    history: &HistoryContext,
    epoch: Epoch,
) -> Result<(), AuthorizationError> {
    if resource_uid.is_empty() {
        return Err(AuthorizationError::InvalidClaims("resource UID is empty"));
    }
    history
        .validate()
        .map_err(AuthorizationError::InvalidClaims)?;
    if history.resource_uid != *resource_uid {
        return Err(AuthorizationError::InvalidClaims(
            "history resource UID does not match the claim",
        ));
    }
    if history.protocol_generation != protocol_generation {
        return Err(AuthorizationError::InvalidClaims(
            "history protocol generation does not match the claim",
        ));
    }
    if epoch.data_loss_number < 0 || epoch.configuration_number < 0 {
        return Err(AuthorizationError::InvalidClaims("epoch is negative"));
    }
    if epoch.data_loss_number != history.data_loss_number {
        return Err(AuthorizationError::InvalidClaims(
            "epoch data-loss number does not match history",
        ));
    }
    Ok(())
}

struct CanonicalEncoder {
    bytes: Vec<u8>,
}

impl CanonicalEncoder {
    fn new(domain: &'static str) -> Self {
        let mut encoder = Self { bytes: Vec::new() };
        encoder
            .string(domain, "authorization domain")
            .expect("fixed authorization domain fits canonical limit");
        encoder.u8(AUTHORIZATION_VERSION_V1);
        encoder
    }

    fn finish(self) -> Vec<u8> {
        self.bytes
    }

    fn u8(&mut self, value: u8) {
        self.bytes.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn u64(&mut self, value: u64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn i64(&mut self, value: i64) {
        self.bytes.extend_from_slice(&value.to_be_bytes());
    }

    fn bytes(&mut self, value: &[u8], field: &'static str) -> Result<(), AuthorizationError> {
        if value.len() > MAX_CANONICAL_FIELD_LENGTH {
            return Err(AuthorizationError::FieldTooLong { field });
        }
        let length =
            u32::try_from(value.len()).map_err(|_| AuthorizationError::FieldTooLong { field })?;
        self.u32(length);
        self.bytes.extend_from_slice(value);
        Ok(())
    }

    fn string(&mut self, value: &str, field: &'static str) -> Result<(), AuthorizationError> {
        self.bytes(value.as_bytes(), field)
    }

    fn option<T>(
        &mut self,
        value: Option<&T>,
        encode: impl FnOnce(&mut Self, &T) -> Result<(), AuthorizationError>,
    ) -> Result<(), AuthorizationError> {
        match value {
            Some(value) => {
                self.u8(1);
                encode(self, value)
            }
            None => {
                self.u8(0);
                Ok(())
            }
        }
    }

    fn replica_identity(&mut self, identity: &ReplicaIdentity) -> Result<(), AuthorizationError> {
        self.i64(identity.replica_id.value());
        self.string(identity.instance_id.as_str(), "replica instance ID")?;
        self.string(
            identity.agent_generation.as_str(),
            "replica agent generation",
        )
    }

    fn replica_role(&mut self, role: ReplicaRole) {
        self.u8(match role {
            ReplicaRole::Primary => 1,
            ReplicaRole::ActiveSecondary => 2,
            ReplicaRole::IdleSecondary => 3,
            ReplicaRole::None => 4,
        });
    }

    fn history(&mut self, history: &HistoryContext) -> Result<(), AuthorizationError> {
        self.string(history.resource_uid.as_str(), "history resource UID")?;
        self.u64(history.protocol_generation.value());
        self.string(history.history_id.as_str(), "history ID")?;
        self.i64(history.data_loss_number);
        Ok(())
    }

    fn epoch(&mut self, epoch: Epoch) {
        self.i64(epoch.data_loss_number);
        self.i64(epoch.configuration_number);
    }

    fn storage_incarnation(
        &mut self,
        storage: &ReplicaStorageIncarnationContext,
    ) -> Result<(), AuthorizationError> {
        self.replica_identity(&storage.replica)?;
        match &storage.storage {
            ReplicaStorageIdentity::Persistent { pod_uid, pvc_uid } => {
                self.u8(1);
                self.string(pod_uid.as_str(), "storage Pod UID")?;
                self.string(pvc_uid.as_str(), "storage PVC UID")?;
            }
            ReplicaStorageIdentity::Volatile { identity } => {
                self.u8(2);
                self.string(identity, "volatile storage identity")?;
            }
        }
        self.string(storage.storage_generation.as_str(), "storage generation")
    }

    fn build_endpoint(
        &mut self,
        endpoint: &BuildAuthorizationEndpointV1,
    ) -> Result<(), AuthorizationError> {
        self.storage_incarnation(&endpoint.storage_incarnation)?;
        self.string(
            endpoint.process_session_id.as_str(),
            "build endpoint process session",
        )?;
        self.string(
            &endpoint.replication_address,
            "build endpoint replication address",
        )?;
        self.replica_role(endpoint.role);
        Ok(())
    }

    fn peer_endpoint(
        &mut self,
        endpoint: &PeerSessionEndpointV1,
    ) -> Result<(), AuthorizationError> {
        self.replica_identity(&endpoint.replica)?;
        self.string(
            endpoint.process_session_id.as_str(),
            "peer endpoint process session",
        )?;
        self.string(
            &endpoint.replication_address,
            "peer endpoint replication address",
        )?;
        self.replica_role(endpoint.role);
        Ok(())
    }
}

struct CanonicalDecoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> CanonicalDecoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn fixed<const N: usize>(
        &mut self,
        field: &'static str,
    ) -> Result<[u8; N], AuthorizationError> {
        let end = self
            .offset
            .checked_add(N)
            .ok_or(AuthorizationError::Truncated { field })?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(AuthorizationError::Truncated { field })?;
        self.offset = end;
        Ok(value
            .try_into()
            .expect("canonical fixed slice has exact length"))
    }

    fn u8(&mut self, field: &'static str) -> Result<u8, AuthorizationError> {
        Ok(self.fixed::<1>(field)?[0])
    }

    fn u32(&mut self, field: &'static str) -> Result<u32, AuthorizationError> {
        Ok(u32::from_be_bytes(self.fixed(field)?))
    }

    fn u64(&mut self, field: &'static str) -> Result<u64, AuthorizationError> {
        Ok(u64::from_be_bytes(self.fixed(field)?))
    }

    fn i64(&mut self, field: &'static str) -> Result<i64, AuthorizationError> {
        Ok(i64::from_be_bytes(self.fixed(field)?))
    }

    fn bytes(&mut self, field: &'static str) -> Result<&'a [u8], AuthorizationError> {
        let length = usize::try_from(self.u32(field)?)
            .map_err(|_| AuthorizationError::FieldTooLong { field })?;
        if length > MAX_CANONICAL_FIELD_LENGTH {
            return Err(AuthorizationError::FieldTooLong { field });
        }
        let end = self
            .offset
            .checked_add(length)
            .ok_or(AuthorizationError::Truncated { field })?;
        let value = self
            .bytes
            .get(self.offset..end)
            .ok_or(AuthorizationError::Truncated { field })?;
        self.offset = end;
        Ok(value)
    }

    fn length_prefixed_fixed<const N: usize>(
        &mut self,
        field: &'static str,
    ) -> Result<[u8; N], AuthorizationError> {
        let value = self.bytes(field)?;
        value
            .try_into()
            .map_err(|_| AuthorizationError::InvalidLength {
                field,
                expected: N,
                actual: value.len(),
            })
    }

    fn string(&mut self, field: &'static str) -> Result<String, AuthorizationError> {
        std::str::from_utf8(self.bytes(field)?)
            .map(str::to_owned)
            .map_err(|_| AuthorizationError::InvalidUtf8 { field })
    }

    fn expect_header(
        &mut self,
        domain: &'static str,
        context: &'static str,
    ) -> Result<(), AuthorizationError> {
        if self.string("authorization domain")? != domain {
            return Err(AuthorizationError::UnknownDomain { context });
        }
        let version = self.u8("authorization version")?;
        if version != AUTHORIZATION_VERSION_V1 {
            return Err(AuthorizationError::UnknownVersion { context, version });
        }
        Ok(())
    }

    fn option<T>(
        &mut self,
        field: &'static str,
        decode: impl FnOnce(&mut Self) -> Result<T, AuthorizationError>,
    ) -> Result<Option<T>, AuthorizationError> {
        match self.u8(field)? {
            0 => Ok(None),
            1 => decode(self).map(Some),
            tag => Err(AuthorizationError::UnknownTag { field, tag }),
        }
    }

    fn replica_identity(&mut self) -> Result<ReplicaIdentity, AuthorizationError> {
        Ok(ReplicaIdentity {
            replica_id: crate::protocol::types::ReplicaId::new(self.i64("replica ID")?),
            instance_id: crate::protocol::types::ReplicaInstanceId::new(
                self.string("replica instance ID")?,
            ),
            agent_generation: crate::protocol::types::AgentGeneration::new(
                self.string("replica agent generation")?,
            ),
        })
    }

    fn replica_role(&mut self) -> Result<ReplicaRole, AuthorizationError> {
        match self.u8("replica role")? {
            1 => Ok(ReplicaRole::Primary),
            2 => Ok(ReplicaRole::ActiveSecondary),
            3 => Ok(ReplicaRole::IdleSecondary),
            4 => Ok(ReplicaRole::None),
            tag => Err(AuthorizationError::UnknownTag {
                field: "replica role",
                tag,
            }),
        }
    }

    fn history(&mut self) -> Result<HistoryContext, AuthorizationError> {
        Ok(HistoryContext {
            resource_uid: ResourceUid::new(self.string("history resource UID")?),
            protocol_generation: PublicProtocolGeneration::new(
                self.u64("history protocol generation")?,
            ),
            history_id: crate::protocol::types::HistoryId::new(self.string("history ID")?),
            data_loss_number: self.i64("history data-loss number")?,
        })
    }

    fn epoch(&mut self) -> Result<Epoch, AuthorizationError> {
        Ok(Epoch::new(
            self.i64("epoch data-loss number")?,
            self.i64("epoch configuration number")?,
        ))
    }

    fn storage_incarnation(
        &mut self,
    ) -> Result<ReplicaStorageIncarnationContext, AuthorizationError> {
        let replica = self.replica_identity()?;
        let storage = match self.u8("storage identity")? {
            1 => ReplicaStorageIdentity::Persistent {
                pod_uid: crate::protocol::types::PodUid::new(self.string("storage Pod UID")?),
                pvc_uid: crate::protocol::types::PvcUid::new(self.string("storage PVC UID")?),
            },
            2 => ReplicaStorageIdentity::Volatile {
                identity: self.string("volatile storage identity")?,
            },
            tag => {
                return Err(AuthorizationError::UnknownTag {
                    field: "storage identity",
                    tag,
                });
            }
        };
        Ok(ReplicaStorageIncarnationContext {
            replica,
            storage,
            storage_generation: StorageGeneration::new(self.string("storage generation")?),
        })
    }

    fn build_endpoint(&mut self) -> Result<BuildAuthorizationEndpointV1, AuthorizationError> {
        Ok(BuildAuthorizationEndpointV1 {
            storage_incarnation: self.storage_incarnation()?,
            process_session_id: ProcessSessionId::new(
                self.string("build endpoint process session")?,
            ),
            replication_address: self.string("build endpoint replication address")?,
            role: self.replica_role()?,
        })
    }

    fn peer_endpoint(&mut self) -> Result<PeerSessionEndpointV1, AuthorizationError> {
        Ok(PeerSessionEndpointV1 {
            replica: self.replica_identity()?,
            process_session_id: ProcessSessionId::new(
                self.string("peer endpoint process session")?,
            ),
            replication_address: self.string("peer endpoint replication address")?,
            role: self.replica_role()?,
        })
    }

    fn finish(self) -> Result<(), AuthorizationError> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(AuthorizationError::TrailingBytes)
        }
    }
}

fn encode_build_claims(claims: &BuildAuthorizationClaimsV1) -> Result<Vec<u8>, AuthorizationError> {
    claims.validate()?;
    let mut encoder = CanonicalEncoder::new(BUILD_CLAIMS_DOMAIN_V1);
    encoder.string(claims.resource_uid.as_str(), "resource UID")?;
    encoder.u64(claims.protocol_generation.value());
    encoder.bytes(claims.key_id.as_bytes(), "authorization key ID")?;
    encoder.history(&claims.history)?;
    encoder.epoch(claims.epoch);
    encoder.string(claims.operation_id.as_str(), "operation ID")?;
    encoder.string(claims.build_id.as_str(), "build ID")?;
    encoder.build_endpoint(&claims.source)?;
    encoder.build_endpoint(&claims.target)?;
    Ok(encoder.finish())
}

fn decode_build_claims(bytes: &[u8]) -> Result<BuildAuthorizationClaimsV1, AuthorizationError> {
    let mut decoder = CanonicalDecoder::new(bytes);
    decoder.expect_header(BUILD_CLAIMS_DOMAIN_V1, "build claims")?;
    let claims = BuildAuthorizationClaimsV1 {
        resource_uid: ResourceUid::new(decoder.string("resource UID")?),
        protocol_generation: PublicProtocolGeneration::new(decoder.u64("protocol generation")?),
        key_id: AuthorizationKeyId::from_bytes(
            decoder.length_prefixed_fixed("authorization key ID")?,
        ),
        history: decoder.history()?,
        epoch: decoder.epoch()?,
        operation_id: OperationId::new(decoder.string("operation ID")?),
        build_id: OperationId::new(decoder.string("build ID")?),
        source: decoder.build_endpoint()?,
        target: decoder.build_endpoint()?,
    };
    decoder.finish()?;
    claims.validate()?;
    if encode_build_claims(&claims)? != bytes {
        return Err(AuthorizationError::NonCanonicalEncoding("build claims"));
    }
    Ok(claims)
}

fn build_signing_bytes(claims: &BuildAuthorizationClaimsV1) -> Result<Vec<u8>, AuthorizationError> {
    let claims = encode_build_claims(claims)?;
    let mut encoder = CanonicalEncoder::new(BUILD_ENVELOPE_DOMAIN_V1);
    encoder.bytes(&claims, "build claims payload")?;
    Ok(encoder.finish())
}

fn decode_build_envelope(bytes: &[u8]) -> Result<BuildAuthorizationV1, AuthorizationError> {
    let mut decoder = CanonicalDecoder::new(bytes);
    decoder.expect_header(BUILD_ENVELOPE_DOMAIN_V1, "build envelope")?;
    let claims_bytes = decoder.bytes("build claims payload")?;
    let signature =
        AuthorizationSignature::from_bytes(decoder.fixed::<64>("authorization signature")?);
    decoder.finish()?;
    let claims = BuildAuthorizationClaimsV1::from_canonical_bytes(claims_bytes)?;
    let authorization = BuildAuthorizationV1 { claims, signature };
    if authorization.canonical_bytes()? != bytes {
        return Err(AuthorizationError::PayloadReencodingMismatch(
            "build authorization",
        ));
    }
    Ok(authorization)
}

fn encode_peer_claims(
    claims: &PeerSessionAuthorizationClaimsV1,
) -> Result<Vec<u8>, AuthorizationError> {
    claims.validate()?;
    let mut encoder = CanonicalEncoder::new(PEER_CLAIMS_DOMAIN_V1);
    encoder.string(claims.resource_uid.as_str(), "resource UID")?;
    encoder.u64(claims.protocol_generation.value());
    encoder.bytes(claims.key_id.as_bytes(), "authorization key ID")?;
    encoder.history(&claims.history)?;
    encoder.epoch(claims.epoch);
    encoder.string(
        claims.current_configuration_id.as_str(),
        "current configuration ID",
    )?;
    encoder.option(
        claims.previous_configuration_id.as_ref(),
        |encoder, configuration_id| {
            encoder.string(configuration_id.as_str(), "previous configuration ID")
        },
    )?;
    encoder.peer_endpoint(&claims.primary)?;
    encoder.peer_endpoint(&claims.secondary)?;
    encoder.u64(claims.configuration_revision);
    Ok(encoder.finish())
}

fn decode_peer_claims(
    bytes: &[u8],
) -> Result<PeerSessionAuthorizationClaimsV1, AuthorizationError> {
    let mut decoder = CanonicalDecoder::new(bytes);
    decoder.expect_header(PEER_CLAIMS_DOMAIN_V1, "peer claims")?;
    let claims = PeerSessionAuthorizationClaimsV1 {
        resource_uid: ResourceUid::new(decoder.string("resource UID")?),
        protocol_generation: PublicProtocolGeneration::new(decoder.u64("protocol generation")?),
        key_id: AuthorizationKeyId::from_bytes(
            decoder.length_prefixed_fixed("authorization key ID")?,
        ),
        history: decoder.history()?,
        epoch: decoder.epoch()?,
        current_configuration_id: ConfigurationId::new(decoder.string("current configuration ID")?),
        previous_configuration_id: decoder.option(
            "previous configuration ID option",
            |decoder| {
                decoder
                    .string("previous configuration ID")
                    .map(ConfigurationId::new)
            },
        )?,
        primary: decoder.peer_endpoint()?,
        secondary: decoder.peer_endpoint()?,
        configuration_revision: decoder.u64("configuration revision")?,
    };
    decoder.finish()?;
    claims.validate()?;
    if encode_peer_claims(&claims)? != bytes {
        return Err(AuthorizationError::NonCanonicalEncoding("peer claims"));
    }
    Ok(claims)
}

fn peer_signing_bytes(
    claims: &PeerSessionAuthorizationClaimsV1,
) -> Result<Vec<u8>, AuthorizationError> {
    let claims = encode_peer_claims(claims)?;
    let mut encoder = CanonicalEncoder::new(PEER_ENVELOPE_DOMAIN_V1);
    encoder.bytes(&claims, "peer claims payload")?;
    Ok(encoder.finish())
}

fn decode_peer_envelope(bytes: &[u8]) -> Result<PeerSessionAuthorizationV1, AuthorizationError> {
    let mut decoder = CanonicalDecoder::new(bytes);
    decoder.expect_header(PEER_ENVELOPE_DOMAIN_V1, "peer envelope")?;
    let claims_bytes = decoder.bytes("peer claims payload")?;
    let signature =
        AuthorizationSignature::from_bytes(decoder.fixed::<64>("authorization signature")?);
    decoder.finish()?;
    let claims = PeerSessionAuthorizationClaimsV1::from_canonical_bytes(claims_bytes)?;
    let authorization = PeerSessionAuthorizationV1 { claims, signature };
    if authorization.canonical_bytes()? != bytes {
        return Err(AuthorizationError::PayloadReencodingMismatch(
            "peer session authorization",
        ));
    }
    Ok(authorization)
}
