use super::proto;
use crate::protocol::public_operations::*;

pub(crate) async fn resolve_replica_publication(
    address: &str,
    bearer_token: &str,
    query: &ReplicaDiscoveryQuery,
) -> crate::Result<Option<ReplicaPublication>> {
    let fail = |error: String| crate::RuntimeError::ControlPlane(error);
    if query.format != ORDINARY_ACTION_FORMAT {
        return Err(crate::RuntimeError::AuthorityNotAdmitted);
    }
    let mut client = proto::replica_peer_client::ReplicaPeerClient::connect(address.to_owned())
        .await
        .map_err(|error| fail(error.to_string()))?;
    let mut request = tonic::Request::new(proto::ReplicaDiscoveryRequest {
        protocol_version: ORDINARY_ACTION_FORMAT,
        canonical_json: serde_json::to_vec(query).map_err(|error| fail(error.to_string()))?,
    });
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {bearer_token}")
            .parse()
            .map_err(|_| crate::RuntimeError::AuthorityNotAdmitted)?,
    );
    let response = client
        .discover(request)
        .await
        .map_err(|error| fail(error.to_string()))?
        .into_inner();
    if response.protocol_version != ORDINARY_ACTION_FORMAT {
        return Err(crate::RuntimeError::AuthorityNotAdmitted);
    }
    let publication: Option<ReplicaPublication> = serde_json::from_slice(&response.canonical_json)
        .map_err(|error| fail(error.to_string()))?;
    if let Some(publication) = &publication {
        publication.validate().map_err(|error| fail(error.into()))?;
        if publication.scope.history != query.history
            || publication.scope.storage.replica != query.replica
            || query
                .process_session_id
                .as_ref()
                .is_some_and(|expected| &publication.scope.process_session_id != expected)
        {
            return Err(crate::RuntimeError::AuthorityNotAdmitted);
        }
    }
    Ok(publication)
}
