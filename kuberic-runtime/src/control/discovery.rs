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
        .map_err(|error| crate::RuntimeError::PeerUnavailable(error.to_string()))?;
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
        .map_err(peer_observation_error)?
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
pub(crate) fn peer_observation_error(error: tonic::Status) -> crate::RuntimeError {
    let mut unavailable = matches!(
        error.code(),
        tonic::Code::Unavailable | tonic::Code::DeadlineExceeded | tonic::Code::Cancelled
    );
    if error.code() == tonic::Code::Unknown {
        let mut source = std::error::Error::source(&error);
        while let Some(cause) = source {
            // h2 keeps its typed I/O cause behind get_io(), not Error::source().
            let io = cause.downcast_ref::<std::io::Error>().or_else(|| {
                cause
                    .downcast_ref::<h2::Error>()
                    .and_then(h2::Error::get_io)
            });
            if cause.downcast_ref::<tonic::transport::Error>().is_some()
                || io.is_some_and(|io| {
                    matches!(
                        io.kind(),
                        std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::ConnectionRefused
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::NotConnected
                            | std::io::ErrorKind::TimedOut
                            | std::io::ErrorKind::UnexpectedEof
                    )
                })
            {
                unavailable = true;
                break;
            }
            source = cause.source();
        }
    }
    if unavailable {
        crate::RuntimeError::PeerUnavailable(error.to_string())
    } else {
        crate::RuntimeError::ControlPlane(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BrokenWire;
    impl tokio::io::AsyncRead for BrokenWire {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
        }
    }
    impl tokio::io::AsyncWrite for BrokenWire {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::from(std::io::ErrorKind::BrokenPipe)))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn real_h2_hidden_io_is_unavailable_but_protocol_rejection_is_not() {
        let error = match h2::client::handshake(BrokenWire).await {
            Err(error) => error,
            Ok(_) => panic!("the actual HTTP/2 handshake must fail at its I/O owner"),
        };
        assert_eq!(
            error.get_io().unwrap().kind(),
            std::io::ErrorKind::BrokenPipe
        );
        assert!(
            std::error::Error::source(&error).is_none(),
            "exercise the opaque-cause boundary"
        );
        let mut status = tonic::Status::unknown("opaque HTTP/2 body failure");
        status.set_source(std::sync::Arc::new(error));
        assert!(matches!(
            peer_observation_error(status),
            crate::RuntimeError::PeerUnavailable(_)
        ));

        let mut status = tonic::Status::unknown("opaque HTTP/2 protocol failure");
        status.set_source(std::sync::Arc::new(h2::Error::from(
            h2::Reason::PROTOCOL_ERROR,
        )));
        assert!(matches!(
            peer_observation_error(status),
            crate::RuntimeError::ControlPlane(_)
        ));
    }

    #[derive(Debug)]
    struct WrappedIo(std::io::Error);
    impl std::fmt::Display for WrappedIo {
        fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            output.write_str("opaque nested observation failure")
        }
    }
    impl std::error::Error for WrappedIo {
        fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
            Some(&self.0)
        }
    }

    #[test]
    fn nested_typed_broken_pipe_is_unavailable_but_opaque_unknown_and_permission_are_not() {
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::ConnectionReset,
        ] {
            let mut status = tonic::Status::unknown("opaque nested observation failure");
            status.set_source(std::sync::Arc::new(WrappedIo(std::io::Error::from(kind))));
            assert_eq!(status.code(), tonic::Code::Unknown);
            assert!(matches!(
                peer_observation_error(status),
                crate::RuntimeError::PeerUnavailable(_)
            ));
        }
        let mut status = tonic::Status::unknown("opaque nested observation failure");
        status.set_source(std::sync::Arc::new(WrappedIo(std::io::Error::from(
            std::io::ErrorKind::PermissionDenied,
        ))));
        assert!(matches!(
            peer_observation_error(status),
            crate::RuntimeError::ControlPlane(_)
        ));
        assert!(
            matches!(
                peer_observation_error(tonic::Status::unknown("broken pipe")),
                crate::RuntimeError::ControlPlane(_)
            ),
            "error wording alone is not typed transport evidence"
        );
        let mut denied = tonic::Status::permission_denied("positive authority rejection");
        denied.set_source(std::sync::Arc::new(WrappedIo(std::io::Error::from(
            std::io::ErrorKind::BrokenPipe,
        ))));
        assert!(matches!(
            peer_observation_error(denied),
            crate::RuntimeError::ControlPlane(_)
        ));
    }

    #[test]
    fn unavailable_observation_does_not_include_authority_or_protocol_rejection() {
        for code in [
            tonic::Code::Unavailable,
            tonic::Code::DeadlineExceeded,
            tonic::Code::Cancelled,
        ] {
            assert!(matches!(
                peer_observation_error(tonic::Status::new(code, "transport cut")),
                crate::RuntimeError::PeerUnavailable(_)
            ));
        }
        for code in [
            tonic::Code::PermissionDenied,
            tonic::Code::Unauthenticated,
            tonic::Code::FailedPrecondition,
            tonic::Code::InvalidArgument,
            tonic::Code::Unknown,
        ] {
            assert!(matches!(
                peer_observation_error(tonic::Status::new(code, "positive rejection")),
                crate::RuntimeError::ControlPlane(_)
            ));
        }
    }
}
