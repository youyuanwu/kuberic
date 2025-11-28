use tonic::{Request, Response, Status};
use tracing::{debug, info};

pub use hello_world::greeter_server::{Greeter, GreeterServer};
use hello_world::{HelloReply, HelloRequest, PodNameReply, PodNameRequest};
use xedio_shared::proto::hello_world;

#[derive(Debug, Default)]
pub struct MyGreeter {}

#[tonic::async_trait]
impl Greeter for MyGreeter {
    async fn say_hello(
        &self,
        request: Request<HelloRequest>,
    ) -> Result<Response<HelloReply>, Status> {
        let name = request.into_inner().name;
        debug!("Received say_hello request for name: {}", name);

        let reply = HelloReply {
            message: format!("Hello {}!", name),
        };

        Ok(Response::new(reply))
    }

    async fn get_pod_name(
        &self,
        _request: Request<PodNameRequest>,
    ) -> Result<Response<PodNameReply>, Status> {
        let pod_name = std::env::var("HOSTNAME").unwrap_or_else(|_| "unknown".to_string());

        info!("Returning pod name: {}", pod_name);

        let reply = PodNameReply { pod_name };

        Ok(Response::new(reply))
    }
}
