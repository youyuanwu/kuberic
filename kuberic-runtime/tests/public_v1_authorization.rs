use base64::prelude::{BASE64_STANDARD, Engine as _};
use ed25519_dalek::{Signer as _, SigningKey};
use kuberic_runtime::protocol::authorization::{
    AuthorizationError, AuthorizationKeyId, AuthorizationSignature, AuthorizationVerificationKey,
    BuildAuthorizationClaimsV1, BuildAuthorizationEndpointV1, BuildAuthorizationV1,
    PeerSessionAuthorizationClaimsV1, PeerSessionAuthorizationV1, PeerSessionEndpointV1,
};
use kuberic_runtime::protocol::public_operations::{
    PublicBuildInput, PublicOperationClass, PublicOperationIntent, PublicOperationPreviewIdentity,
    PublicOperationProgram, PublicOperationReplayDisposition, PublicProgress, PublicProgressRole,
    PublicV1AgentReport,
};
use kuberic_runtime::protocol::types::{
    AccessStatus, AgentGeneration, ConfigurationId, Epoch, HistoryContext, HistoryId, OperationId,
    PodUid, ProcessSessionId, PublicProtocolGeneration, PvcUid, ReplicaId, ReplicaIdentity,
    ReplicaRole, ReplicaStorageIdentity, ReplicaStorageIncarnationContext, ResourceUid,
    StorageGeneration,
};
use kuberic_runtime::replicator::{
    PublicAuthorizationTrust, ReplicaInformation, ReplicatorSettings,
};
use sha2::{Digest, Sha256};

const BUILD_CLAIMS_DOMAIN: &str = "kuberic-build-authorization-claims-v1";
const BUILD_ENVELOPE_DOMAIN: &str = "kuberic-build-authorization-envelope-v1";
const PEER_CLAIMS_DOMAIN: &str = "kuberic-peer-session-authorization-claims-v1";
const PEER_ENVELOPE_DOMAIN: &str = "kuberic-peer-session-authorization-envelope-v1";

struct TestSigningKey(SigningKey);

impl std::fmt::Debug for TestSigningKey {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("TestSigningKey(<redacted>)")
    }
}

impl TestSigningKey {
    fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }

    fn verification_key(&self) -> AuthorizationVerificationKey {
        AuthorizationVerificationKey::from_bytes(self.0.verifying_key().to_bytes()).unwrap()
    }

    fn key_id(&self) -> AuthorizationKeyId {
        self.verification_key().key_id()
    }

    fn sign_build_authorization(
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

    fn sign_peer_session_authorization(
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

fn signing_key() -> TestSigningKey {
    TestSigningKey::from_seed([7; 32])
}

fn wrong_signing_key() -> TestSigningKey {
    TestSigningKey::from_seed([9; 32])
}

fn replica(id: i64, name: &str) -> ReplicaIdentity {
    ReplicaIdentity {
        replica_id: ReplicaId::new(id),
        instance_id: kuberic_runtime::protocol::types::ReplicaInstanceId::new(format!(
            "{name}-instance"
        )),
        agent_generation: AgentGeneration::new(format!("{name}-agent")),
    }
}

fn history() -> HistoryContext {
    HistoryContext {
        resource_uid: ResourceUid::new("resource-123"),
        protocol_generation: PublicProtocolGeneration::new(11),
        history_id: HistoryId::new("history-abc"),
        data_loss_number: 4,
    }
}

fn build_endpoint(name: &str, id: i64, role: ReplicaRole) -> BuildAuthorizationEndpointV1 {
    BuildAuthorizationEndpointV1 {
        storage_incarnation: ReplicaStorageIncarnationContext {
            replica: replica(id, name),
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

fn build_claims(key_id: AuthorizationKeyId) -> BuildAuthorizationClaimsV1 {
    BuildAuthorizationClaimsV1 {
        resource_uid: ResourceUid::new("resource-123"),
        protocol_generation: PublicProtocolGeneration::new(11),
        key_id,
        history: history(),
        epoch: Epoch::new(4, 12),
        operation_id: OperationId::new("operation-42"),
        build_id: OperationId::new("build-77"),
        source: build_endpoint("source", 1, ReplicaRole::Primary),
        target: build_endpoint("target", 2, ReplicaRole::IdleSecondary),
    }
}

fn peer_endpoint(name: &str, id: i64, role: ReplicaRole) -> PeerSessionEndpointV1 {
    PeerSessionEndpointV1 {
        replica: replica(id, name),
        process_session_id: ProcessSessionId::new(format!("{name}-session-5")),
        replication_address: format!("https://10.0.0.{id}:7000"),
        role,
    }
}

fn peer_claims(key_id: AuthorizationKeyId) -> PeerSessionAuthorizationClaimsV1 {
    PeerSessionAuthorizationClaimsV1 {
        resource_uid: ResourceUid::new("resource-123"),
        protocol_generation: PublicProtocolGeneration::new(11),
        key_id,
        history: history(),
        epoch: Epoch::new(4, 12),
        current_configuration_id: ConfigurationId::new("cfg-current-12"),
        previous_configuration_id: Some(ConfigurationId::new("cfg-previous-11")),
        primary: peer_endpoint("source", 1, ReplicaRole::Primary),
        secondary: peer_endpoint("target", 2, ReplicaRole::ActiveSecondary),
        configuration_revision: 19,
    }
}

fn signed_build() -> (AuthorizationVerificationKey, BuildAuthorizationV1) {
    let signing_key = signing_key();
    let verification_key = signing_key.verification_key();
    let authorization = signing_key
        .sign_build_authorization(build_claims(verification_key.key_id()))
        .unwrap();
    (verification_key, authorization)
}

fn signed_peer() -> (AuthorizationVerificationKey, PeerSessionAuthorizationV1) {
    let signing_key = signing_key();
    let verification_key = signing_key.verification_key();
    let authorization = signing_key
        .sign_peer_session_authorization(peer_claims(verification_key.key_id()))
        .unwrap();
    (verification_key, authorization)
}

fn find(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
        .unwrap()
}

fn version_offset(domain: &str) -> usize {
    4 + domain.len()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn public_v1_authorization_golden_vectors_are_shared_by_signing_and_verification() {
    let (verification_key, build) = signed_build();
    let (_, peer) = signed_peer();
    let build_bytes = build.canonical_bytes().unwrap();
    let peer_bytes = peer.canonical_bytes().unwrap();

    assert_eq!(
        verification_key.to_base64(),
        "6kpsY+KcUgq+9VB7Ey7F+ZVHdq6+vnuSQh7qaRRG0iw="
    );
    assert_eq!(
        verification_key.key_id().to_base64(),
        "/oEsEvOrTOasXbaaw1L5BssbEe9D+zPiUu9/9VImOIk="
    );
    assert_eq!(
        hex(&build.claims.canonical_bytes().unwrap()),
        "000000256b7562657269632d6275696c642d617574686f72697a6174696f6e2d636c61696d732d7631010000000c7265736f757263652d313233000000000000000b00000020fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff5522638890000000c7265736f757263652d313233000000000000000b0000000b686973746f72792d61626300000000000000040000000000000004000000000000000c0000000c6f7065726174696f6e2d3432000000086275696c642d373700000000000000010000000f736f757263652d696e7374616e63650000000c736f757263652d6167656e74010000000a736f757263652d706f640000000a736f757263652d70766300000010736f757263652d73746f726167652d3300000010736f757263652d73657373696f6e2d350000001568747470733a2f2f31302e302e302e313a373030300100000000000000020000000f7461726765742d696e7374616e63650000000c7461726765742d6167656e74010000000a7461726765742d706f640000000a7461726765742d707663000000107461726765742d73746f726167652d33000000107461726765742d73657373696f6e2d350000001568747470733a2f2f31302e302e302e323a3730303003"
    );
    assert_eq!(
        BASE64_STANDARD.encode(&build_bytes),
        "AAAAJ2t1YmVyaWMtYnVpbGQtYXV0aG9yaXphdGlvbi1lbnZlbG9wZS12MQEAAAHVAAAAJWt1YmVyaWMtYnVpbGQtYXV0aG9yaXphdGlvbi1jbGFpbXMtdjEBAAAADHJlc291cmNlLTEyMwAAAAAAAAALAAAAIP6BLBLzq0zmrF22msNS+QbLGxHvQ/sz4lLvf/VSJjiJAAAADHJlc291cmNlLTEyMwAAAAAAAAALAAAAC2hpc3RvcnktYWJjAAAAAAAAAAQAAAAAAAAABAAAAAAAAAAMAAAADG9wZXJhdGlvbi00MgAAAAhidWlsZC03NwAAAAAAAAABAAAAD3NvdXJjZS1pbnN0YW5jZQAAAAxzb3VyY2UtYWdlbnQBAAAACnNvdXJjZS1wb2QAAAAKc291cmNlLXB2YwAAABBzb3VyY2Utc3RvcmFnZS0zAAAAEHNvdXJjZS1zZXNzaW9uLTUAAAAVaHR0cHM6Ly8xMC4wLjAuMTo3MDAwAQAAAAAAAAACAAAAD3RhcmdldC1pbnN0YW5jZQAAAAx0YXJnZXQtYWdlbnQBAAAACnRhcmdldC1wb2QAAAAKdGFyZ2V0LXB2YwAAABB0YXJnZXQtc3RvcmFnZS0zAAAAEHRhcmdldC1zZXNzaW9uLTUAAAAVaHR0cHM6Ly8xMC4wLjAuMjo3MDAwA6ODym1zDQleTgw2sTxX5VCdZrf0Q+wSMorFo4iROABdJWNSTjUnJ2iSMi/oapm2O+HiznPzq1zJhcksAVuyiQo="
    );
    assert_eq!(
        build.signature.to_base64(),
        "o4PKbXMNCV5ODDaxPFflUJ1mt/RD7BIyisWjiJE4AF0lY1JONScnaJIyL+hqmbY74eLOc/OrXMmFySwBW7KJCg=="
    );
    assert_eq!(
        build.audit_digest().unwrap().to_base64(),
        "xUzvtO+Ef71api56PAJSPTcCtyZ0YnM/thJ0PbFSgRk="
    );
    assert_eq!(
        hex(&peer.claims.canonical_bytes().unwrap()),
        "0000002c6b7562657269632d706565722d73657373696f6e2d617574686f72697a6174696f6e2d636c61696d732d7631010000000c7265736f757263652d313233000000000000000b00000020fe812c12f3ab4ce6ac5db69ac352f906cb1b11ef43fb33e252ef7ff5522638890000000c7265736f757263652d313233000000000000000b0000000b686973746f72792d61626300000000000000040000000000000004000000000000000c0000000e6366672d63757272656e742d3132010000000f6366672d70726576696f75732d313100000000000000010000000f736f757263652d696e7374616e63650000000c736f757263652d6167656e7400000010736f757263652d73657373696f6e2d350000001568747470733a2f2f31302e302e302e313a373030300100000000000000020000000f7461726765742d696e7374616e63650000000c7461726765742d6167656e74000000107461726765742d73657373696f6e2d350000001568747470733a2f2f31302e302e302e323a37303030020000000000000013"
    );
    assert_eq!(
        BASE64_STANDARD.encode(&peer_bytes),
        "AAAALmt1YmVyaWMtcGVlci1zZXNzaW9uLWF1dGhvcml6YXRpb24tZW52ZWxvcGUtdjEBAAABjAAAACxrdWJlcmljLXBlZXItc2Vzc2lvbi1hdXRob3JpemF0aW9uLWNsYWltcy12MQEAAAAMcmVzb3VyY2UtMTIzAAAAAAAAAAsAAAAg/oEsEvOrTOasXbaaw1L5BssbEe9D+zPiUu9/9VImOIkAAAAMcmVzb3VyY2UtMTIzAAAAAAAAAAsAAAALaGlzdG9yeS1hYmMAAAAAAAAABAAAAAAAAAAEAAAAAAAAAAwAAAAOY2ZnLWN1cnJlbnQtMTIBAAAAD2NmZy1wcmV2aW91cy0xMQAAAAAAAAABAAAAD3NvdXJjZS1pbnN0YW5jZQAAAAxzb3VyY2UtYWdlbnQAAAAQc291cmNlLXNlc3Npb24tNQAAABVodHRwczovLzEwLjAuMC4xOjcwMDABAAAAAAAAAAIAAAAPdGFyZ2V0LWluc3RhbmNlAAAADHRhcmdldC1hZ2VudAAAABB0YXJnZXQtc2Vzc2lvbi01AAAAFWh0dHBzOi8vMTAuMC4wLjI6NzAwMAIAAAAAAAAAE4ooqBOJr4Q4Emd7m9+xp5V9tXZnmUv37tTF95i+/3eZfsUYx20IokVioVvYRW45stlA58mK78AcHhgzHNNXSAo="
    );
    assert_eq!(
        peer.signature.to_base64(),
        "iiioE4mvhDgSZ3ub37GnlX21dmeZS/fu1MX3mL7/d5l+xRjHbQiiRWKhW9hFbjmy2UDnyYrvwBweGDMc01dICg=="
    );
    assert_eq!(
        peer.audit_digest().unwrap().to_base64(),
        "rIJSowF9rpWsENmfPdCUPBGUi4282BKFw8l6PsXzESI="
    );

    assert_eq!(
        BuildAuthorizationV1::from_canonical_bytes(&build_bytes).unwrap(),
        build
    );
    assert_eq!(
        PeerSessionAuthorizationV1::from_canonical_bytes(&peer_bytes).unwrap(),
        peer
    );
    verification_key.verify_build_authorization(&build).unwrap();
    verification_key
        .verify_peer_session_authorization(&peer)
        .unwrap();
}

#[test]
fn public_v1_authorization_rejects_tampering_before_mutation() {
    fn mutate_provider(
        trust: &PublicAuthorizationTrust,
        source: &BuildAuthorizationEndpointV1,
        authorization: &BuildAuthorizationV1,
        mutations: &mut u64,
    ) -> Result<(), AuthorizationError> {
        let mut descriptor = ReplicaInformation::idle_build(
            authorization.claims.build_id.clone(),
            authorization
                .claims
                .target
                .storage_incarnation
                .replica
                .clone(),
            authorization.claims.target.replication_address.clone(),
        );
        descriptor.process_session_id = authorization.claims.target.process_session_id.clone();
        descriptor.storage_incarnation =
            Some(authorization.claims.target.storage_incarnation.clone());
        descriptor.build_authorization = Some(authorization.clone());
        descriptor
            .validate_build_authorization(
                trust,
                &authorization.claims.history,
                authorization.claims.epoch,
                &authorization.claims.operation_id,
                source,
            )
            .map_err(|_| AuthorizationError::InvalidClaims("runtime descriptor mismatch"))?;
        *mutations += 1;
        Ok(())
    }

    let (verification_key, authorization) = signed_build();
    let trust = PublicAuthorizationTrust {
        resource_uid: authorization.claims.resource_uid.clone(),
        protocol_generation: authorization.claims.protocol_generation,
        verification_key,
        key_id: authorization.claims.key_id,
    };
    let settings = ReplicatorSettings {
        replication_address: authorization.claims.source.replication_address.clone(),
        public_authorization: Some(trust.clone()),
    };
    assert!(settings.public_authorization.is_some());
    let mut tampered = authorization.clone();
    tampered.claims.target.process_session_id = ProcessSessionId::new("predecessor-session");
    let mut mutations = 0;

    assert!(
        mutate_provider(
            &trust,
            &authorization.claims.source,
            &tampered,
            &mut mutations
        )
        .is_err()
    );
    assert_eq!(mutations, 0);
    let mut stale_claims = authorization.claims.clone();
    stale_claims.source.process_session_id = ProcessSessionId::new("predecessor-source-session");
    let stale = signing_key()
        .sign_build_authorization(stale_claims)
        .unwrap();
    assert!(mutate_provider(&trust, &authorization.claims.source, &stale, &mut mutations).is_err());
    assert_eq!(mutations, 0);
    mutate_provider(
        &trust,
        &authorization.claims.source,
        &authorization,
        &mut mutations,
    )
    .unwrap();
    assert_eq!(mutations, 1);
}

#[test]
fn public_v1_authorization_runtime_never_receives_private_seed() {
    let seed = [0x5a; 32];
    let encoded_seed = BASE64_STANDARD.encode(seed);
    let hex_seed = hex(&seed);
    let signing_key = TestSigningKey::from_seed(seed);
    let verification_key = signing_key.verification_key();
    let authorization = signing_key
        .sign_build_authorization(build_claims(verification_key.key_id()))
        .unwrap();
    let json = serde_json::to_string(&authorization).unwrap();
    let canonical = authorization.canonical_bytes().unwrap();
    let mut invalid = authorization.clone();
    invalid.signature = AuthorizationSignature::from_bytes([0; 64]);
    let error = invalid.verify(&verification_key).unwrap_err().to_string();
    let trust = PublicAuthorizationTrust {
        resource_uid: authorization.claims.resource_uid.clone(),
        protocol_generation: authorization.claims.protocol_generation,
        verification_key,
        key_id: verification_key.key_id(),
    };
    let settings = ReplicatorSettings {
        replication_address: authorization.claims.source.replication_address.clone(),
        public_authorization: Some(trust),
    };
    let mut descriptor = ReplicaInformation::idle_build(
        authorization.claims.build_id.clone(),
        authorization
            .claims
            .target
            .storage_incarnation
            .replica
            .clone(),
        authorization.claims.target.replication_address.clone(),
    );
    descriptor.process_session_id = authorization.claims.target.process_session_id.clone();
    descriptor.storage_incarnation = Some(authorization.claims.target.storage_incarnation.clone());
    descriptor.build_authorization = Some(authorization.clone());
    let program = PublicOperationProgram::Build(PublicBuildInput {
        attempt: authorization.claims.build_id.clone(),
        replica: authorization
            .claims
            .target
            .storage_incarnation
            .replica
            .clone(),
        process_session_id: authorization.claims.target.process_session_id.clone(),
        replication_address: authorization.claims.target.replication_address.clone(),
        history: Some(authorization.claims.history.clone()),
        authorization: None,
        authorization_verification_key: Some(verification_key),
        build_authorization: Some(authorization.clone()),
    });
    let stored_intent = PublicOperationIntent {
        preview: PublicOperationPreviewIdentity::public_v1(
            authorization.claims.protocol_generation.value(),
        ),
        operation_id: authorization.claims.operation_id.clone(),
        revision: 1,
        process_session_id: authorization.claims.source.process_session_id.clone(),
        class: PublicOperationClass::Build {
            target: authorization
                .claims
                .target
                .storage_incarnation
                .replica
                .replica_id,
        },
        input_digest: program.digest(),
        replay_disposition: PublicOperationReplayDisposition::RetainBuildSuccessRetireAmbiguity,
        lifecycle: None,
        program: Some(program),
    };
    let report = PublicV1AgentReport {
        identity: PublicOperationPreviewIdentity::public_v1(
            authorization.claims.protocol_generation.value(),
        ),
        resource_uid: authorization.claims.resource_uid.clone(),
        replica: authorization
            .claims
            .source
            .storage_incarnation
            .replica
            .clone(),
        process_session_id: authorization.claims.source.process_session_id.clone(),
        report_sequence: 1,
        role: ReplicaRole::Primary,
        read_status: AccessStatus::Granted,
        write_status: AccessStatus::Granted,
        history: authorization.claims.history.clone(),
        storage_incarnation: authorization.claims.source.storage_incarnation.clone(),
        progress: PublicProgress {
            role: PublicProgressRole::Primary,
            first: 1,
            last: 1,
        },
        data_loss_recovery: None,
        completed_operations: Vec::new(),
        extensions: Default::default(),
    };
    let runtime_surfaces = [
        ("authorization JSON", json.clone()),
        ("replicator settings", format!("{settings:?}")),
        ("build descriptor", format!("{descriptor:?}")),
        (
            "durable operation intent",
            serde_json::to_string(&stored_intent).unwrap(),
        ),
        ("agent report", serde_json::to_string(&report).unwrap()),
        ("diagnostic", error.clone()),
    ];

    assert_eq!(format!("{signing_key:?}"), "TestSigningKey(<redacted>)");
    for (surface, value) in runtime_surfaces {
        assert!(
            !value.contains(&encoded_seed) && !value.contains(&hex_seed),
            "{surface} leaked private seed material"
        );
        assert!(
            !value
                .as_bytes()
                .windows(seed.len())
                .any(|window| window == seed),
            "{surface} leaked raw private seed bytes"
        );
    }
    assert!(!canonical.windows(seed.len()).any(|window| window == seed));
    assert_ne!(verification_key.to_bytes(), seed);
}

#[test]
fn public_v1_authorization_rejects_every_build_claim_field_tamper() {
    let (verification_key, authorization) = signed_build();
    let mut tampered = Vec::new();

    macro_rules! tamper {
        ($change:expr) => {{
            let mut value = authorization.clone();
            $change(&mut value.claims);
            tampered.push(value);
        }};
    }

    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.resource_uid =
            ResourceUid::new("other-resource")
    );
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.protocol_generation =
            PublicProtocolGeneration::new(12)
    );
    tamper!(|claims: &mut BuildAuthorizationClaimsV1| claims.key_id = wrong_signing_key().key_id());
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.history.resource_uid =
            ResourceUid::new("other-resource")
    );
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.history.protocol_generation =
            PublicProtocolGeneration::new(12)
    );
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.history.history_id =
            HistoryId::new("other-history")
    );
    tamper!(|claims: &mut BuildAuthorizationClaimsV1| claims.history.data_loss_number = 5);
    tamper!(|claims: &mut BuildAuthorizationClaimsV1| claims.epoch.data_loss_number = 5);
    tamper!(|claims: &mut BuildAuthorizationClaimsV1| claims.epoch.configuration_number = 13);
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.operation_id =
            OperationId::new("other-operation")
    );
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.build_id = OperationId::new("other-build")
    );
    tamper!(|claims: &mut BuildAuthorizationClaimsV1| claims
        .source
        .storage_incarnation
        .replica
        .replica_id = ReplicaId::new(9));
    tamper!(|claims: &mut BuildAuthorizationClaimsV1| claims
        .source
        .storage_incarnation
        .replica
        .instance_id =
        kuberic_runtime::protocol::types::ReplicaInstanceId::new("other-instance"));
    tamper!(|claims: &mut BuildAuthorizationClaimsV1| claims
        .source
        .storage_incarnation
        .replica
        .agent_generation =
        AgentGeneration::new("other-agent"));
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.source.storage_incarnation.storage =
            ReplicaStorageIdentity::Volatile {
                identity: "other-source-storage".to_owned()
            }
    );
    tamper!(|claims: &mut BuildAuthorizationClaimsV1| claims
        .source
        .storage_incarnation
        .storage_generation =
        StorageGeneration::new("other-source-generation"));
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.source.process_session_id =
            ProcessSessionId::new("other-source-session")
    );
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.source.replication_address =
            "https://other-source:7000".to_owned()
    );
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.source.role = ReplicaRole::ActiveSecondary
    );
    tamper!(|claims: &mut BuildAuthorizationClaimsV1| claims
        .target
        .storage_incarnation
        .replica
        .replica_id = ReplicaId::new(8));
    tamper!(|claims: &mut BuildAuthorizationClaimsV1| claims
        .target
        .storage_incarnation
        .replica
        .instance_id =
        kuberic_runtime::protocol::types::ReplicaInstanceId::new("other-instance"));
    tamper!(|claims: &mut BuildAuthorizationClaimsV1| claims
        .target
        .storage_incarnation
        .replica
        .agent_generation =
        AgentGeneration::new("other-agent"));
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.target.storage_incarnation.storage =
            ReplicaStorageIdentity::Volatile {
                identity: "other-target-storage".to_owned()
            }
    );
    tamper!(|claims: &mut BuildAuthorizationClaimsV1| claims
        .target
        .storage_incarnation
        .storage_generation =
        StorageGeneration::new("other-target-generation"));
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.target.process_session_id =
            ProcessSessionId::new("other-target-session")
    );
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.target.replication_address =
            "https://other-target:7000".to_owned()
    );
    tamper!(
        |claims: &mut BuildAuthorizationClaimsV1| claims.target.role = ReplicaRole::ActiveSecondary
    );

    assert_eq!(tampered.len(), 27);
    for value in tampered {
        assert!(value.verify(&verification_key).is_err());
    }
}

#[test]
fn public_v1_authorization_rejects_every_peer_claim_field_tamper() {
    let (verification_key, authorization) = signed_peer();
    let mut tampered = Vec::new();

    macro_rules! tamper {
        ($change:expr) => {{
            let mut value = authorization.clone();
            $change(&mut value.claims);
            tampered.push(value);
        }};
    }

    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.resource_uid =
            ResourceUid::new("other-resource")
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.protocol_generation =
            PublicProtocolGeneration::new(12)
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.key_id =
            wrong_signing_key().key_id()
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.history.resource_uid =
            ResourceUid::new("other-resource")
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.history.protocol_generation =
            PublicProtocolGeneration::new(12)
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.history.history_id =
            HistoryId::new("other-history")
    );
    tamper!(|claims: &mut PeerSessionAuthorizationClaimsV1| claims.history.data_loss_number = 5);
    tamper!(|claims: &mut PeerSessionAuthorizationClaimsV1| claims.epoch.data_loss_number = 5);
    tamper!(|claims: &mut PeerSessionAuthorizationClaimsV1| claims.epoch.configuration_number = 13);
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.current_configuration_id =
            ConfigurationId::new("other-current")
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.previous_configuration_id = None
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.primary.replica.replica_id =
            ReplicaId::new(9)
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.primary.replica.instance_id =
            kuberic_runtime::protocol::types::ReplicaInstanceId::new("other-instance")
    );
    tamper!(|claims: &mut PeerSessionAuthorizationClaimsV1| claims
        .primary
        .replica
        .agent_generation =
        AgentGeneration::new("other-agent"));
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.primary.process_session_id =
            ProcessSessionId::new("other-primary-session")
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.primary.replication_address =
            "https://other-primary:7000".to_owned()
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.primary.role =
            ReplicaRole::ActiveSecondary
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.secondary.replica.replica_id =
            ReplicaId::new(8)
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.secondary.replica.instance_id =
            kuberic_runtime::protocol::types::ReplicaInstanceId::new("other-instance")
    );
    tamper!(|claims: &mut PeerSessionAuthorizationClaimsV1| claims
        .secondary
        .replica
        .agent_generation =
        AgentGeneration::new("other-agent"));
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.secondary.process_session_id =
            ProcessSessionId::new("other-secondary-session")
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.secondary.replication_address =
            "https://other-secondary:7000".to_owned()
    );
    tamper!(
        |claims: &mut PeerSessionAuthorizationClaimsV1| claims.secondary.role =
            ReplicaRole::IdleSecondary
    );
    tamper!(|claims: &mut PeerSessionAuthorizationClaimsV1| claims.configuration_revision = 20);

    assert_eq!(tampered.len(), 24);
    for value in tampered {
        assert!(value.verify(&verification_key).is_err());
    }
}

#[test]
fn public_v1_authorization_rejects_domain_confusion_wrong_keys_and_key_ids() {
    let (verification_key, build) = signed_build();
    let (_, peer) = signed_peer();

    assert!(
        PeerSessionAuthorizationV1::from_canonical_bytes(&build.canonical_bytes().unwrap())
            .is_err()
    );
    assert!(BuildAuthorizationV1::from_canonical_bytes(&peer.canonical_bytes().unwrap()).is_err());
    assert!(
        PeerSessionAuthorizationClaimsV1::from_canonical_bytes(
            &build.claims.canonical_bytes().unwrap()
        )
        .is_err()
    );
    assert!(
        BuildAuthorizationClaimsV1::from_canonical_bytes(&peer.claims.canonical_bytes().unwrap())
            .is_err()
    );

    let mut crossed_signature = peer.clone();
    crossed_signature.signature = build.signature;
    assert_eq!(
        crossed_signature.verify(&verification_key),
        Err(AuthorizationError::InvalidSignature)
    );

    let wrong_key = wrong_signing_key().verification_key();
    assert_eq!(
        build.verify(&wrong_key),
        Err(AuthorizationError::KeyIdMismatch)
    );

    let mut wrong_key_id = build_claims(wrong_key.key_id());
    assert_eq!(
        signing_key().sign_build_authorization(wrong_key_id.clone()),
        Err(AuthorizationError::KeyIdMismatch)
    );
    wrong_key_id.key_id = verification_key.key_id();
    signing_key()
        .sign_build_authorization(wrong_key_id)
        .unwrap()
        .verify(&verification_key)
        .unwrap();

    let noncanonical_public_key = (0..=u16::MAX)
        .find_map(|prefix| {
            let mut bytes = [0xff; 32];
            bytes[..2].copy_from_slice(&prefix.to_be_bytes());
            AuthorizationVerificationKey::from_bytes(bytes)
                .is_err()
                .then_some(bytes)
        })
        .expect("Ed25519 has invalid compressed points");
    assert_eq!(
        AuthorizationVerificationKey::from_bytes(noncanonical_public_key),
        Err(AuthorizationError::InvalidPublicKey)
    );
}

#[test]
fn public_v1_authorization_rejects_truncation_trailing_and_malformed_bytes() {
    let (_, build) = signed_build();
    let (_, peer) = signed_peer();
    let build_envelope = build.canonical_bytes().unwrap();
    let peer_envelope = peer.canonical_bytes().unwrap();
    let build_claims = build.claims.canonical_bytes().unwrap();
    let peer_claims = peer.claims.canonical_bytes().unwrap();

    for end in 0..build_envelope.len() {
        assert!(BuildAuthorizationV1::from_canonical_bytes(&build_envelope[..end]).is_err());
    }
    for end in 0..peer_envelope.len() {
        assert!(PeerSessionAuthorizationV1::from_canonical_bytes(&peer_envelope[..end]).is_err());
    }
    for end in 0..build_claims.len() {
        assert!(BuildAuthorizationClaimsV1::from_canonical_bytes(&build_claims[..end]).is_err());
    }
    for end in 0..peer_claims.len() {
        assert!(
            PeerSessionAuthorizationClaimsV1::from_canonical_bytes(&peer_claims[..end]).is_err()
        );
    }

    let mut trailing = build_envelope.clone();
    trailing.push(0);
    assert_eq!(
        BuildAuthorizationV1::from_canonical_bytes(&trailing),
        Err(AuthorizationError::TrailingBytes)
    );
    let mut trailing = peer_envelope.clone();
    trailing.push(0);
    assert_eq!(
        PeerSessionAuthorizationV1::from_canonical_bytes(&trailing),
        Err(AuthorizationError::TrailingBytes)
    );

    let mut malformed_length = build_claims.clone();
    malformed_length[..4].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(BuildAuthorizationClaimsV1::from_canonical_bytes(&malformed_length).is_err());

    let mut unknown_version = build_claims.clone();
    unknown_version[version_offset(BUILD_CLAIMS_DOMAIN)] = 2;
    assert!(matches!(
        BuildAuthorizationClaimsV1::from_canonical_bytes(&unknown_version),
        Err(AuthorizationError::UnknownVersion { .. })
    ));
    let mut unknown_version = peer_envelope.clone();
    unknown_version[version_offset(PEER_ENVELOPE_DOMAIN)] = 2;
    assert!(matches!(
        PeerSessionAuthorizationV1::from_canonical_bytes(&unknown_version),
        Err(AuthorizationError::UnknownVersion { .. })
    ));

    let mut invalid_storage_tag = build_claims.clone();
    let source_pod = find(&invalid_storage_tag, b"source-pod");
    invalid_storage_tag[source_pod - 5] = 99;
    assert!(matches!(
        BuildAuthorizationClaimsV1::from_canonical_bytes(&invalid_storage_tag),
        Err(AuthorizationError::UnknownTag {
            field: "storage identity",
            ..
        })
    ));

    let mut invalid_role = build_claims.clone();
    *invalid_role.last_mut().unwrap() = 99;
    assert!(matches!(
        BuildAuthorizationClaimsV1::from_canonical_bytes(&invalid_role),
        Err(AuthorizationError::UnknownTag {
            field: "replica role",
            ..
        })
    ));

    let mut invalid_option = peer_claims.clone();
    let current = find(&invalid_option, b"cfg-current-12");
    invalid_option[current + "cfg-current-12".len()] = 2;
    assert!(matches!(
        PeerSessionAuthorizationClaimsV1::from_canonical_bytes(&invalid_option),
        Err(AuthorizationError::UnknownTag {
            field: "previous configuration ID option",
            ..
        })
    ));

    let mut invalid_utf8 = build_claims.clone();
    let address = find(&invalid_utf8, b"https://10.0.0.1:7000");
    invalid_utf8[address] = 0xff;
    assert!(matches!(
        BuildAuthorizationClaimsV1::from_canonical_bytes(&invalid_utf8),
        Err(AuthorizationError::InvalidUtf8 { .. })
    ));
}

#[test]
fn public_v1_authorization_rejects_duplicate_and_noncanonical_claims() {
    let (_, build) = signed_build();
    let mut duplicate = build.claims.canonical_bytes().unwrap();

    let source_instance = find(&duplicate, b"source-instance");
    let target_instance = find(&duplicate, b"target-instance");
    let source_agent = find(&duplicate, b"source-agent");
    let target_agent = find(&duplicate, b"target-agent");
    duplicate.copy_within(
        source_instance - 12..source_instance - 4,
        target_instance - 12,
    );
    duplicate.copy_within(
        source_instance..source_instance + "source-instance".len(),
        target_instance,
    );
    duplicate.copy_within(
        source_agent..source_agent + "source-agent".len(),
        target_agent,
    );
    assert!(matches!(
        BuildAuthorizationClaimsV1::from_canonical_bytes(&duplicate),
        Err(AuthorizationError::Duplicate { .. })
    ));

    let mut wrong_key_length = build.claims.canonical_bytes().unwrap();
    let key_id = find(&wrong_key_length, build.claims.key_id.as_bytes());
    wrong_key_length[key_id - 4..key_id].copy_from_slice(&31_u32.to_be_bytes());
    assert!(BuildAuthorizationClaimsV1::from_canonical_bytes(&wrong_key_length).is_err());

    let mut wrong_domain = build.claims.canonical_bytes().unwrap();
    let domain = find(&wrong_domain, BUILD_CLAIMS_DOMAIN.as_bytes());
    wrong_domain[domain] ^= 1;
    assert!(matches!(
        BuildAuthorizationClaimsV1::from_canonical_bytes(&wrong_domain),
        Err(AuthorizationError::UnknownDomain { .. })
    ));

    let mut envelope_payload = build.canonical_bytes().unwrap();
    let claims_domain = find(&envelope_payload, BUILD_CLAIMS_DOMAIN.as_bytes());
    envelope_payload[claims_domain] ^= 1;
    assert!(BuildAuthorizationV1::from_canonical_bytes(&envelope_payload).is_err());
}

#[test]
fn public_v1_authorization_json_uses_only_strict_canonical_base64() {
    let (verification_key, build) = signed_build();
    let json = serde_json::to_value(&build).unwrap();
    let round_trip: BuildAuthorizationV1 = serde_json::from_value(json.clone()).unwrap();
    assert_eq!(round_trip, build);
    round_trip.verify(&verification_key).unwrap();

    assert_eq!(
        serde_json::to_string(&verification_key).unwrap(),
        format!("\"{}\"", verification_key.to_base64())
    );
    assert_eq!(
        serde_json::from_str::<AuthorizationVerificationKey>(&format!(
            "\"{}\"",
            verification_key.to_base64()
        ))
        .unwrap(),
        verification_key
    );

    let mut missing_padding = json.clone();
    let signature = missing_padding["signature"].as_str().unwrap().to_owned();
    missing_padding["signature"] =
        serde_json::Value::String(signature.trim_end_matches('=').to_owned());
    assert!(serde_json::from_value::<BuildAuthorizationV1>(missing_padding).is_err());

    let mut whitespace = json.clone();
    whitespace["signature"] =
        serde_json::Value::String(format!(" {}", build.signature.to_base64()));
    assert!(serde_json::from_value::<BuildAuthorizationV1>(whitespace).is_err());

    let mut unknown_field = json;
    unknown_field["privateSeed"] = serde_json::Value::String(BASE64_STANDARD.encode([7; 32]));
    assert!(serde_json::from_value::<BuildAuthorizationV1>(unknown_field).is_err());

    assert!(
        serde_json::from_str::<AuthorizationKeyId>(&format!(
            "\"{}\"",
            BASE64_STANDARD.encode([0; 31])
        ))
        .is_err()
    );
}

#[test]
fn public_v1_authorization_audit_digest_hashes_exact_signed_envelope() {
    let (_, build) = signed_build();
    let (_, peer) = signed_peer();
    assert_eq!(
        build.audit_digest().unwrap().to_bytes(),
        <[u8; 32]>::from(Sha256::digest(build.canonical_bytes().unwrap()))
    );
    assert_eq!(
        peer.audit_digest().unwrap().to_bytes(),
        <[u8; 32]>::from(Sha256::digest(peer.canonical_bytes().unwrap()))
    );
    assert_ne!(build.audit_digest().unwrap(), peer.audit_digest().unwrap());
}

#[test]
fn public_v1_authorization_enforces_claim_invariants_before_signing() {
    let signing_key = signing_key();

    let mut invalid = build_claims(signing_key.key_id());
    invalid.target = invalid.source.clone();
    assert!(matches!(
        signing_key.sign_build_authorization(invalid),
        Err(AuthorizationError::InvalidClaims(_)) | Err(AuthorizationError::Duplicate { .. })
    ));

    let mut invalid = peer_claims(signing_key.key_id());
    invalid.previous_configuration_id = Some(invalid.current_configuration_id.clone());
    assert!(matches!(
        signing_key.sign_peer_session_authorization(invalid),
        Err(AuthorizationError::Duplicate { .. })
    ));

    let mut invalid = peer_claims(signing_key.key_id());
    invalid.configuration_revision = 0;
    assert!(matches!(
        signing_key.sign_peer_session_authorization(invalid),
        Err(AuthorizationError::InvalidClaims(_))
    ));
}

#[test]
fn public_v1_authorization_domain_constants_are_unambiguous() {
    assert_ne!(BUILD_CLAIMS_DOMAIN, BUILD_ENVELOPE_DOMAIN);
    assert_ne!(PEER_CLAIMS_DOMAIN, PEER_ENVELOPE_DOMAIN);
    assert_ne!(BUILD_CLAIMS_DOMAIN, PEER_CLAIMS_DOMAIN);
    assert_ne!(BUILD_ENVELOPE_DOMAIN, PEER_ENVELOPE_DOMAIN);
}
