use std::sync::{Arc, Weak};
use tonic::{Request, Response, Status};

use crate::build::{MAX_ENVELOPE_BYTES, PgBuildRequest, decode, encode};
use crate::proto::pg_data_service_server::{PgDataService, PgDataServiceServer};
use crate::proto::{NativeBuildRequest, NativeBuildResponse, NativeSourceResponse};
use crate::service::PgService;

pub(crate) struct PgCoordination {
    pub local_endpoint: String,
    pub bearer_token: String,
    pub listen_address: Option<std::net::SocketAddr>,
}

pub struct PgDataServiceImpl {
    backend: Backend,
    token: String,
    requests: Arc<RequestTracker>,
}

enum Backend {
    Service(Weak<PgService>),
    Driver(Weak<crate::adapter::PgReplicator>),
}

#[derive(Default)]
pub(crate) struct RequestTracker {
    active: std::sync::atomic::AtomicUsize,
    changed: tokio::sync::Notify,
}

struct RequestGuard<'a>(&'a RequestTracker);

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.0
            .active
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        self.0.changed.notify_one();
    }
}

impl RequestTracker {
    fn enter(&self) -> RequestGuard<'_> {
        self.active
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        RequestGuard(self)
    }

    pub(crate) async fn join(&self) {
        loop {
            let changed = self.changed.notified();
            if self.active.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return;
            }
            changed.await;
        }
    }
}

impl PgDataServiceImpl {
    pub fn new(service: Arc<PgService>, token: String) -> Self {
        Self {
            backend: Backend::Service(Arc::downgrade(&service)),
            token,
            requests: Arc::new(RequestTracker::default()),
        }
    }

    pub(crate) fn for_driver(
        driver: Weak<crate::adapter::PgReplicator>,
        token: String,
        requests: Arc<RequestTracker>,
    ) -> Self {
        Self {
            backend: Backend::Driver(driver),
            token,
            requests,
        }
    }

    fn driver(&self) -> Result<Arc<crate::adapter::PgReplicator>, Status> {
        match &self.backend {
            Backend::Service(service) => service
                .upgrade()
                .ok_or_else(|| Status::unavailable("PostgreSQL application closed"))?
                .driver()
                .cloned(),
            Backend::Driver(driver) => driver
                .upgrade()
                .ok_or_else(|| Status::unavailable("PostgreSQL engine closed")),
        }
    }

    fn authenticate<T>(&self, request: &Request<T>) -> Result<(), Status> {
        let expected = format!("Bearer {}", self.token);
        if self.token.is_empty()
            || request
                .metadata()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                != Some(expected.as_str())
        {
            return Err(Status::unauthenticated(
                "invalid PostgreSQL coordination credentials",
            ));
        }
        Ok(())
    }

    pub fn into_server(self) -> PgDataServiceServer<Self> {
        PgDataServiceServer::new(self)
            .max_decoding_message_size(MAX_ENVELOPE_BYTES + 16)
            .max_encoding_message_size(MAX_ENVELOPE_BYTES + 16)
    }

    fn request(&self, request: Request<NativeBuildRequest>) -> Result<PgBuildRequest, Status> {
        self.authenticate(&request)?;
        let request: PgBuildRequest =
            decode(&request.into_inner().envelope_json).map_err(Status::invalid_argument)?;
        request.validate().map_err(Status::invalid_argument)?;
        Ok(request)
    }
}

#[tonic::async_trait]
impl PgDataService for PgDataServiceImpl {
    async fn public_v1(
        &self,
        request: Request<crate::proto::RecoveryRequest>,
    ) -> Result<Response<crate::proto::RecoveryResponse>, Status> {
        let _request_lifetime = self.requests.enter();
        self.authenticate(&request)?;
        let request =
            decode(&request.into_inner().envelope_json).map_err(Status::invalid_argument)?;
        let response = self
            .driver()?
            .public_request(request)
            .await
            .map_err(|error| Status::failed_precondition(error.to_string()))?;
        Ok(Response::new(crate::proto::RecoveryResponse {
            envelope_json: encode(&response).map_err(Status::internal)?,
        }))
    }

    async fn recover(
        &self,
        request: Request<crate::proto::RecoveryRequest>,
    ) -> Result<Response<crate::proto::RecoveryResponse>, Status> {
        let _request_lifetime = self.requests.enter();
        self.authenticate(&request)?;
        let request =
            decode(&request.into_inner().envelope_json).map_err(Status::invalid_argument)?;
        let observation = self
            .driver()?
            .recovery_request(request)
            .await
            .map_err(|error| Status::failed_precondition(error.to_string()))?;
        Ok(Response::new(crate::proto::RecoveryResponse {
            envelope_json: encode(&observation).map_err(Status::internal)?,
        }))
    }

    async fn build(
        &self,
        request: Request<NativeBuildRequest>,
    ) -> Result<Response<NativeBuildResponse>, Status> {
        let _request_lifetime = self.requests.enter();
        let request = self.request(request)?;
        let progress = self
            .driver()?
            .receive_build(request)
            .await
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        Ok(Response::new(NativeBuildResponse {
            progress_json: encode(&progress).map_err(Status::internal)?,
        }))
    }

    async fn inspect_source(
        &self,
        request: Request<NativeBuildRequest>,
    ) -> Result<Response<NativeSourceResponse>, Status> {
        let _request_lifetime = self.requests.enter();
        let request = self.request(request)?;
        let lineage = self
            .driver()?
            .inspect_source(&request)
            .await
            .map_err(|e| Status::failed_precondition(e.to_string()))?;
        Ok(Response::new(NativeSourceResponse {
            lineage_json: encode(&lineage).map_err(Status::internal)?,
        }))
    }
}
